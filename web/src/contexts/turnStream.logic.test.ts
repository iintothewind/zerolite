import assert from 'node:assert/strict';
import test from 'node:test';

import {
  classifyCompletion,
  initialTurnStreamState,
  reduceTurnFrame,
  streamedText,
  type CompletionInput,
  type CompletionOutcome,
  type TurnStreamFrame,
} from './turnStream.logic.ts';
import type { TurnSegments } from '../lib/turnSegments.ts';

/** Fold a whole frame sequence, collecting every completion the turns emit.
 *  Returns the final state so tests can assert cross-turn reset invariants. */
function runFrames(frames: TurnStreamFrame[]) {
  let state = initialTurnStreamState();
  const completions: CompletionOutcome[] = [];
  for (const frame of frames) {
    const result = reduceTurnFrame(state, frame);
    state = result.state;
    if (result.completion) completions.push(result.completion);
  }
  return { state, completions };
}

// ── The reasoning-only bug this PR fixes ────────────────────────────────────

test('reasoning-only turn commits with empty content and captured thinking', () => {
  // GLM/Qwen/DeepSeek: whole answer arrives as reasoning deltas, content is
  // empty, no tool calls. Before the fix this produced no bubble at all.
  const { completions } = runFrames([
    { type: 'thinking', content: 'The user asks why ' },
    { type: 'thinking', content: 'the 21:00 shutdown ' },
    { type: 'thinking', content: 'did not run.' },
    { type: 'chunk', content: '' },
    { type: 'done', full_response: '' },
  ]);
  assert.equal(completions.length, 1);
  assert.deepEqual(completions[0], {
    kind: 'commit',
    content: '',
    thinking: 'The user asks why the 21:00 shutdown did not run.',
  });
});

// ── Genuinely empty clean completion → exactly one diagnostic ───────────────

test('empty clean completion emits exactly one diagnostic', () => {
  const { completions } = runFrames([{ type: 'done', full_response: '' }]);
  assert.equal(completions.length, 1);
  assert.deepEqual(completions[0], { kind: 'diagnostic' });
});

// ── Tool-only completion → no diagnostic (cards are the record) ─────────────

test('tool-only completion skips: no diagnostic when tools ran', () => {
  const { completions } = runFrames([
    { type: 'tool_call', hasName: true },
    { type: 'done', full_response: '' },
  ]);
  assert.equal(completions.length, 1);
  assert.deepEqual(completions[0], { kind: 'skip' });
});

// ── Normal visible answer → commit with the streamed text ───────────────────

test('normal content commits the concatenated stream', () => {
  const { completions } = runFrames([
    { type: 'chunk', content: 'Round 1: 2 + 3 ' },
    { type: 'chunk', content: '+ 4 = 9' },
    { type: 'done' },
  ]);
  assert.equal(completions.length, 1);
  assert.deepEqual(completions[0], {
    kind: 'commit',
    content: 'Round 1: 2 + 3 + 4 = 9',
    thinking: undefined,
  });
});

// ── #6702: whitespace-only content alongside a tool_call → skip ─────────────

test('whitespace-only content with a tool call skips (issue #6702)', () => {
  const { completions } = runFrames([
    { type: 'chunk', content: '\n\n' },
    { type: 'tool_call', hasName: true },
    { type: 'done', full_response: '' },
  ]);
  assert.equal(completions.length, 1);
  assert.deepEqual(completions[0], { kind: 'skip' });
});

// ── A nameless telemetry tool_call must not flip hadToolCall (#7151) ────────

test('nameless tool_call frame does not count as a tool call', () => {
  // Empty completion after only a nameless (telemetry) tool_call must still
  // diagnose — the turn genuinely produced nothing.
  const { completions } = runFrames([
    { type: 'tool_call', hasName: false },
    { type: 'done', full_response: '' },
  ]);
  assert.deepEqual(completions[0], { kind: 'diagnostic' });
});

// ── chunk_reset drops the draft, keeps the reasoning ────────────────────────

test('chunk_reset discards the draft text but keeps the reasoning', () => {
  // The server restarts the content stream and resends the answer with the
  // terminal frame; reasoning is not resent, so it must survive the reset.
  const { completions } = runFrames([
    { type: 'thinking', content: 'reasoning here' },
    { type: 'chunk', content: 'draft that gets reset' },
    { type: 'chunk_reset' },
    { type: 'done', full_response: 'final answer' },
  ]);
  assert.deepEqual(completions[0], {
    kind: 'commit',
    content: 'final answer',
    thinking: 'reasoning here',
  });
});

test('chunk_reset drops the draft even without an authoritative resend', () => {
  // Nothing else arrives: the discarded draft must not become the answer.
  const { completions } = runFrames([
    { type: 'chunk', content: 'draft that gets reset' },
    { type: 'chunk_reset' },
    { type: 'chunk', content: 'the real answer' },
    { type: 'done' },
  ]);
  assert.deepEqual(completions[0], {
    kind: 'commit',
    content: 'the real answer',
    thinking: undefined,
  });
});

test('a reset draft does not leak into the rendered trajectory', () => {
  // The trajectory is the only store, so what the live view shows and what the
  // commit keeps cannot disagree: a discarded draft is gone from both.
  let state = initialTurnStreamState();
  for (const frame of [
    { type: 'chunk', content: 'draft that gets reset' },
    { type: 'chunk_reset' },
    { type: 'chunk', content: 'the real answer' },
  ] as TurnStreamFrame[]) {
    state = reduceTurnFrame(state, frame).state;
  }
  assert.equal(state.openStep.text, 'the real answer');
});

// ── The invariant the reviewer called out: state resets across turns ────────

test('per-turn state resets between turns', () => {
  // Turn 1: tool-only (skip). Turn 2: reasoning-only (commit). Turn 2 must NOT
  // see turn 1's hadToolCall or leftover buffers.
  const { state, completions } = runFrames([
    // turn 1
    { type: 'chunk', content: '\n\n' },
    { type: 'tool_call', hasName: true },
    { type: 'done', full_response: '' },
    // turn 2
    { type: 'thinking', content: 'fresh reasoning' },
    { type: 'done', full_response: '' },
  ]);
  assert.deepEqual(completions, [
    { kind: 'skip' },
    { kind: 'commit', content: '', thinking: 'fresh reasoning' },
  ]);
  // After the second completion the state is fully fresh.
  assert.deepEqual(state, initialTurnStreamState());
});

test('empty second turn diagnoses instead of inheriting turn 1 tool call', () => {
  // Regression guard: if hadToolCall leaked across turns, this empty second
  // turn would wrongly skip instead of diagnosing.
  const { completions } = runFrames([
    { type: 'tool_call', hasName: true },
    { type: 'done', full_response: 'answer one' },
    { type: 'done', full_response: '' },
  ]);
  assert.deepEqual(completions, [
    { kind: 'commit', content: 'answer one', thinking: undefined },
    { kind: 'diagnostic' },
  ]);
});

test('error then new turn discards reasoning accumulated before the error', () => {
  // Regression sequence from review: the first turn streams reasoning, then
  // fails. Starting a new turn must not let that stale reasoning turn an empty
  // completion into a reasoning-only assistant message.
  const { state, completions } = runFrames([
    { type: 'thinking', content: 'stale reasoning' },
    { type: 'chunk_reset' },
    { type: 'error' },
    { type: 'turn_start' },
    { type: 'done', full_response: '' },
  ]);
  assert.deepEqual(completions, [{ kind: 'diagnostic' }]);
  assert.deepEqual(state, initialTurnStreamState());
});

// ── classifyCompletion fallback chain (full_response ?? content ?? streamed) ─

/** A classification input carrying only streamed text, the common case. */
function streamed(content: string): CompletionInput {
  return { streamedContent: content, streamedThinking: '', hadToolCall: false };
}

test('classifyCompletion falls back to frame content then the streamed text', () => {
  // full_response present wins.
  assert.deepEqual(
    classifyCompletion(streamed('streamed'), { full_response: 'authoritative' }),
    { kind: 'commit', content: 'authoritative', thinking: undefined },
  );
  // no full_response → frame content.
  assert.deepEqual(
    classifyCompletion(streamed('streamed'), { content: 'frame body' }),
    { kind: 'commit', content: 'frame body', thinking: undefined },
  );
  // neither → the live-streamed text.
  assert.deepEqual(
    classifyCompletion(streamed('streamed'), {}),
    { kind: 'commit', content: 'streamed', thinking: undefined },
  );
});

test('streamed reasoning commits a turn whose content is empty', () => {
  assert.deepEqual(
    classifyCompletion(
      { streamedContent: '', streamedThinking: 'only reasoning', hadToolCall: false },
      { full_response: '' },
    ),
    { kind: 'commit', content: '', thinking: 'only reasoning' },
  );
});

// ── Step trajectory (the message-flow view) ─────────────────────────────────
//
// The `usage` frame is the only server-authoritative step boundary: it is
// emitted once the response is accepted, *before* that step's tool-call events.
// These cases pin that ordering, because inferring boundaries from tool calls
// alone cannot tell a final answer from a step that has not called anything yet.

/** Fold frames and return the trajectory after each fold, plus the last one. */
function runSegments(frames: TurnStreamFrame[]) {
  let state = initialTurnStreamState();
  const snapshots: TurnSegments[] = [];
  const completions: CompletionOutcome[] = [];
  for (const frame of frames) {
    const result = reduceTurnFrame(state, frame);
    state = result.state;
    snapshots.push(result.segments);
    if (result.completion) completions.push(result.completion);
  }
  return { state, snapshots, completions, last: snapshots[snapshots.length - 1]! };
}

test('a usage frame closes a step and the tool call attaches to it', () => {
  const { last } = runSegments([
    { type: 'turn_start' },
    { type: 'thinking', content: 'plan' },
    { type: 'chunk', content: 'let me look' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1', args: { cmd: 'ls' } } },
  ]);
  assert.equal(last.steps.length, 1);
  assert.deepEqual(last.steps[0], {
    thinking: 'plan',
    text: 'let me look',
    toolCalls: [{ name: 'shell', id: 't1', args: { cmd: 'ls' } }],
  });
});

test('several tool calls in one step are parallel calls, not several steps', () => {
  const { last } = runSegments([
    { type: 'chunk', content: 'running both' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1' } },
    { type: 'tool_call', hasName: true, call: { name: 'file_read', id: 't2' } },
  ]);
  assert.equal(last.steps.length, 1);
  assert.deepEqual(last.steps[0]!.toolCalls.map((c) => c.id), ['t1', 't2']);
});

test('tool results correlate by id, including out of order', () => {
  const { last } = runSegments([
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1' } },
    { type: 'tool_call', hasName: true, call: { name: 'file_read', id: 't2' } },
    { type: 'tool_result', id: 't2', output: 'second' },
    { type: 'tool_result', id: 't1', output: 'first' },
  ]);
  assert.deepEqual(
    last.steps[0]!.toolCalls.map((c) => c.output),
    ['first', 'second'],
  );
});

test('a step with no tool calls becomes the final answer', () => {
  const { last, completions } = runSegments([
    { type: 'turn_start' },
    { type: 'chunk', content: 'checking' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1' } },
    { type: 'thinking', content: 'now I can answer' },
    { type: 'chunk', content: 'the answer' },
    { type: 'usage' },
    { type: 'done', full_response: 'the answer' },
  ]);
  assert.equal(last.steps.length, 1, 'only the tool-calling step stays in the group');
  assert.equal(last.steps[0]!.toolCalls.length, 1);
  // The answer step leaves the trajectory: the committed bubble carries it.
  assert.deepEqual(completions[0], {
    kind: 'commit',
    content: 'the answer',
    thinking: 'now I can answer',
  });
});

test('a turn that ends on a tool call keeps that step in the trajectory', () => {
  const { last, completions } = runSegments([
    { type: 'chunk', content: 'calling' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1' } },
    { type: 'done', full_response: '' },
  ]);
  assert.equal(last.steps.length, 1, 'a tool-calling step is never the answer');
  assert.deepEqual(completions[0], { kind: 'skip' }, 'the cards are the record');
});

test('the classification input is derived from the trajectory', () => {
  // One store: what `classifyCompletion` reads is what the steps hold, so a
  // closed step's text keeps counting after the next step has opened.
  let state = initialTurnStreamState();
  for (const frame of [
    { type: 'chunk', content: 'first' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1' } },
    { type: 'chunk', content: 'second' },
  ] as TurnStreamFrame[]) {
    state = reduceTurnFrame(state, frame).state;
  }
  assert.equal(streamedText(state), 'firstsecond', 'closed steps plus the open one');
});

test('steps keep the order the frames arrived in', () => {
  const { last, completions } = runSegments([
    { type: 'chunk', content: 'a' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1' } },
    { type: 'chunk', content: 'b' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'file_read', id: 't2' } },
    { type: 'chunk', content: 'c' },
    { type: 'usage' },
    { type: 'done', full_response: 'c' },
  ]);
  assert.deepEqual(
    last.steps.map((s) => s.text),
    ['a', 'b'],
  );
  assert.deepEqual(completions[0], {
    kind: 'commit',
    content: 'c',
    thinking: undefined,
  });
});

test('an empty step is not a step', () => {
  // `usage` frames with nothing accumulated before them (a provider that
  // reports usage for an empty response) must not create blank segments.
  const { last, completions } = runSegments([
    { type: 'usage' },
    { type: 'usage' },
    { type: 'done', full_response: 'hi' },
  ]);
  assert.equal(last.steps.length, 0);
  assert.deepEqual(completions[0], {
    kind: 'commit',
    content: 'hi',
    thinking: undefined,
  });
});

test('the trajectory is live-only: a fresh turn starts empty', () => {
  const first = runSegments([
    { type: 'chunk', content: 'a' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1' } },
    { type: 'done', full_response: '' },
  ]);
  assert.equal(first.last.steps.length, 1);
  const second = runSegments([
    { type: 'chunk', content: 'fresh' },
    { type: 'usage' },
    { type: 'done', full_response: 'fresh' },
  ]);
  assert.equal(second.last.steps.length, 0);
  assert.deepEqual(second.completions[0], {
    kind: 'commit',
    content: 'fresh',
    thinking: undefined,
  });
});

test('aborted keeps the partial answer and the trajectory', () => {
  const { state, completions, last } = runSegments([
    { type: 'chunk', content: 'checking' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1' } },
    { type: 'chunk', content: 'partial' },
    { type: 'aborted' },
  ]);
  assert.deepEqual(completions, [{ kind: 'commit', content: 'partial', thinking: undefined }]);
  assert.equal(last.steps.length, 1);
  assert.equal(last.steps[0]!.text, 'checking');
  assert.equal(last.steps[0]!.toolCalls.length, 1);
  assert.deepEqual(state, initialTurnStreamState());
});

test('aborted with nothing streamed commits nothing', () => {
  const { completions, last } = runSegments([{ type: 'aborted' }]);
  assert.equal(completions.length, 0);
  assert.equal(last.steps.length, 0);
});

test('error keeps the whole trajectory in the group and commits nothing', () => {
  const { completions, last } = runSegments([
    { type: 'chunk', content: 'checking' },
    { type: 'usage' },
    { type: 'tool_call', hasName: true, call: { name: 'shell', id: 't1' } },
    { type: 'chunk', content: 'partial' },
    { type: 'error' },
  ]);
  assert.equal(completions.length, 0);
  assert.equal(last.steps.length, 2);
  assert.equal(last.steps[1]!.text, 'partial');
});
