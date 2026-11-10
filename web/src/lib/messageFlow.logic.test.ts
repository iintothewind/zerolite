import assert from 'node:assert/strict';
import test from 'node:test';

import type { StepSegment, TurnSegments } from './turnSegments.ts';
import {
  countGroupMessages,
  countToolCalls,
  groupMessages,
  pendingOutputIds,
  splitLiveTurn,
  type FlowMessage,
  type RenderBlock,
} from './messageFlow.logic.ts';

function msg(over: Partial<FlowMessage> & { id: string }): FlowMessage {
  return { role: 'agent', content: '', timestamp: new Date(0), ...over };
}

function segments(over: Partial<TurnSegments> = {}): TurnSegments {
  return { steps: [], ...over };
}

const toolStep = (id: string, text = '') => ({
  thinking: '',
  text,
  toolCalls: [{ name: 'shell', id }],
});

const kinds = (blocks: RenderBlock[]) => blocks.map((b) => b.kind);
const ids = (blocks: RenderBlock[]) =>
  blocks.map((b) => (b.kind === 'group' ? b.id : b.message.id));
const stepsOf = (block: RenderBlock | undefined) => {
  assert.ok(block && (block.kind === 'turn' || block.kind === 'group'));
  return block.segments;
};

// ── Turns with a trajectory ─────────────────────────────────────────────────

test('a live turn becomes one block that absorbs its tool cards', () => {
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'analyze this' }),
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1', output: 'ok' } }),
    msg({ id: 't2', toolCall: { name: 'file_read', id: 'c2', output: 'ok' } }),
    msg({
      id: 'a1',
      content: 'the answer',
      markdown: true,
      segments: segments({ steps: [toolStep('c1'), toolStep('c2')] }),
    }),
  ]);

  assert.deepEqual(kinds(blocks), ['user', 'turn']);
  const turn = blocks[1]!;
  assert.equal(turn.kind, 'turn');
  if (turn.kind !== 'turn') return;
  assert.equal(turn.message.id, 'a1');
  assert.equal(turn.segments.steps.length, 2, 'the loose cards live inside the group now');
});

test('a turn with no tool calls stays a plain bubble', () => {
  // No empty group, no header reading `0 次工具调用`.
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'hi' }),
    msg({ id: 'a1', content: 'hello', segments: segments() }),
  ]);
  assert.deepEqual(kinds(blocks), ['user', 'plain']);
});

// ── Turns folded from the message list ──────────────────────────────────────

test('a hydrated turn folds into one group with its narration as a step', () => {
  const card = { name: 'shell', id: 'c1', output: 'out' };
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'hi' }),
    msg({ id: 'a1', content: 'hello', markdown: true }),
    msg({ id: 't1', toolCall: card }),
    msg({ id: 'a2', content: 'done', markdown: true }),
  ]);
  assert.deepEqual(kinds(blocks), ['user', 'turn']);
  const turn = blocks[1]!;
  assert.equal(turn.kind === 'turn' && turn.message.id, 'a2');
  const segs = stepsOf(turn);
  assert.deepEqual(segs.steps, [{ thinking: '', text: 'hello', toolCalls: [card] }]);
  assert.equal(countToolCalls(segs), 1);
  assert.equal(countGroupMessages(segs), 1);
});

test('a turn with cards and no answer is one answer-less group', () => {
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'hi' }),
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 't2', toolCall: { name: 'shell', id: 'c2' } }),
  ]);
  assert.deepEqual(kinds(blocks), ['user', 'group']);
  assert.equal(blocks[1]!.kind === 'group' && blocks[1]!.id, 't1');
  assert.equal(countToolCalls(stepsOf(blocks[1])), 2);
});

test('a stopped or failed turn keeps its trajectory as a group', () => {
  const carrier = msg({
    id: 'c',
    content: '',
    segments: segments({
      steps: [
        { thinking: '', text: 'looking', toolCalls: [{ name: 'shell', id: 'c1' }] },
        toolStep('c2'),
      ],
    }),
  });
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'hi' }),
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 't2', toolCall: { name: 'shell', id: 'c2' } }),
    carrier,
  ]);
  assert.deepEqual(kinds(blocks), ['user', 'group']);
  // The absorbed cards are gone, so the carrier keys the group.
  assert.equal(blocks[1]!.kind === 'group' && blocks[1]!.id, 'c');
  assert.equal(countToolCalls(stepsOf(blocks[1])), 2);
  assert.equal(countGroupMessages(stepsOf(blocks[1])), 1);
});

test('a notice inside a turn neither splits it nor duplicates its cards', () => {
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'hi' }),
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 't2', toolCall: { name: 'shell', id: 'c2' } }),
    msg({ id: 'n1', content: 'context trimmed', notice: true }),
    msg({ id: 't3', toolCall: { name: 'shell', id: 'c3' } }),
    msg({
      id: 'a1',
      content: 'answer',
      segments: segments({ steps: [toolStep('c1'), toolStep('c2'), toolStep('c3')] }),
    }),
  ]);
  assert.deepEqual(kinds(blocks), ['user', 'notice', 'turn']);
  assert.equal(stepsOf(blocks[2]).steps.length, 3);
});

test('an error bubble after a failed turn stays a plain bubble', () => {
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'hi' }),
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 'c', content: '', segments: segments({ steps: [toolStep('c1')] }) }),
    msg({ id: 'err', content: 'Error: x' }),
  ]);
  assert.deepEqual(kinds(blocks), ['user', 'group', 'plain']);
});

test('narration from server rows loses the gateway timestamp prefix', () => {
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'hi' }),
    msg({ id: 'a1', content: '[2026-11-09 10:00:00 UTC] looking' }),
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 'a2', content: 'done' }),
  ]);
  assert.equal(stepsOf(blocks[1]).steps[0]!.text, 'looking');
});

test('text-only runs stay plain even with several bubbles', () => {
  const blocks = groupMessages([
    msg({ id: 'a1', content: 'cron one' }),
    msg({ id: 'a2', content: 'cron two' }),
  ]);
  assert.deepEqual(kinds(blocks), ['plain', 'plain']);
});

test('notice rows keep their own block kind', () => {
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'hi' }),
    msg({ id: 'n1', content: 'context trimmed', notice: true }),
    msg({ id: 'a1', content: 'ok' }),
  ]);
  assert.deepEqual(kinds(blocks), ['user', 'notice', 'plain']);
});

test('user-separated turns each fold their own cards', () => {
  const blocks = groupMessages([
    msg({ id: 'u1', role: 'user', content: 'one' }),
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 't2', toolCall: { name: 'shell', id: 'c2' } }),
    msg({ id: 'a1', content: 'first' }),
    msg({ id: 'u2', role: 'user', content: 'two' }),
    msg({ id: 't3', toolCall: { name: 'shell', id: 'c3' } }),
    msg({ id: 'a2', content: 'second' }),
  ]);
  assert.deepEqual(kinds(blocks), ['user', 'turn', 'user', 'turn']);
  assert.equal(countToolCalls(stepsOf(blocks[1])), 2);
  assert.equal(countToolCalls(stepsOf(blocks[3])), 1);
});

test('two live turns each get their own group', () => {
  const blocks = groupMessages([
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 'a1', content: 'one', segments: segments({ steps: [toolStep('c1')] }) }),
    msg({ id: 't2', toolCall: { name: 'shell', id: 'c2' } }),
    msg({ id: 'a2', content: 'two', segments: segments({ steps: [toolStep('c2')] }) }),
  ]);
  assert.deepEqual(kinds(blocks), ['turn', 'turn']);
});

// ── The tool-activity toggle ────────────────────────────────────────────────

test('hiding tool activity hides the loose cards and the group body', () => {
  const messages = [
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 'a1', content: 'one', segments: segments({ steps: [toolStep('c1')] }) }),
    msg({ id: 'a2', content: 'plain answer' }),
  ];
  assert.deepEqual(kinds(groupMessages(messages, { showToolActivity: true })), [
    'turn',
    'plain',
  ]);
  assert.deepEqual(kinds(groupMessages(messages, { showToolActivity: false })), [
    'plain',
    'plain',
  ]);
});

// ── Header counts ───────────────────────────────────────────────────────────

test('a card the live group already carries is not rendered twice', () => {
  // While a turn streams there is no committed message to absorb its cards, so
  // the in-flight group carries them — and the loose card of the same
  // `tool_call_id` has to go, or every call renders twice for the duration.
  const messages = [
    msg({ id: 'u1', role: 'user', content: 'analyze this' }),
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 't2', toolCall: { name: 'file_read', id: 'c2' } }),
  ];

  const live = groupMessages(messages, { liveSteps: [toolStep('c1')] });
  assert.deepEqual(ids(live), ['u1', 't2'], 'the claimed card is dropped');
  assert.deepEqual(kinds(live), ['user', 'group']);

  // No live turn, nothing claimed: both cards fold into the turn's group.
  const settled = groupMessages(messages);
  assert.deepEqual(kinds(settled), ['user', 'group']);
  assert.equal(countToolCalls(stepsOf(settled[1])), 2);
});

// ── The in-flight turn ──────────────────────────────────────────────────────

const open = (over: Partial<StepSegment> = {}): StepSegment => ({
  thinking: '',
  text: '',
  toolCalls: [],
  ...over,
});

test('the answer is the step being written, and the rest is the group', () => {
  const split = splitLiveTurn({
    steps: [toolStep('c1', 'checking the file')],
    open: open({ text: 'the answer so far' }),
  });
  assert.equal(split.answer?.text, 'the answer so far');
  assert.deepEqual(split.groupSteps.map((step) => step.text), ['checking the file']);
});

test('a tool-free turn has no group at all while it streams', () => {
  // The commonest turn there is: one step, no calls. If the just-closed step
  // did not count as the answer it would sit in the group, and the reader would
  // watch a `0 次工具调用` header appear and vanish.
  const split = splitLiveTurn({
    steps: [open({ text: 'hello' })],
    open: open(),
  });
  assert.equal(split.answer?.text, 'hello');
  assert.deepEqual(split.groupSteps, []);
});

test('a closed step stops being the answer once the turn continues past it', () => {
  // The transition the view has to get right: the same closed step is the
  // answer while nothing follows it, and becomes trajectory the moment the next
  // step starts writing.
  const closed = [open({ text: 'let me think' })];

  const waiting = splitLiveTurn({ steps: closed, open: open() });
  assert.equal(waiting.answer?.text, 'let me think');
  assert.deepEqual(waiting.groupSteps, []);

  const continued = splitLiveTurn({ steps: closed, open: open({ text: 'the answer' }) });
  assert.equal(continued.answer?.text, 'the answer');
  assert.deepEqual(continued.groupSteps.map((step) => step.text), ['let me think']);
});

test('a turn that ends on a tool call has no answer', () => {
  const split = splitLiveTurn({
    steps: [open({ text: 'checking', toolCalls: [{ name: 'shell', id: 'c1' }] })],
    open: open(),
  });
  assert.equal(split.answer, null);
  assert.equal(split.groupSteps.length, 1, 'its cards are the record');
});

test('an idle turn splits into nothing', () => {
  assert.deepEqual(splitLiveTurn({ steps: [], open: open() }), { groupSteps: [], answer: null });
});

test('the header counts calls and messages', () => {
  const seg = segments({
    steps: [
      { thinking: 'thought', text: '', toolCalls: [{ name: 'shell', id: 'c1' }] },
      { thinking: '', text: '', toolCalls: [{ name: 'a', id: 'c2' }, { name: 'b', id: 'c3' }] },
      { thinking: '', text: 'explaining', toolCalls: [] },
    ],
  });
  assert.equal(countToolCalls(seg), 3, 'parallel calls count individually');
  assert.equal(countGroupMessages(seg), 2, 'steps carrying thinking or text');
});

test('an empty group counts zero', () => {
  assert.equal(countToolCalls(segments()), 0);
  assert.equal(countGroupMessages(segments()), 0);
});

test('pendingOutputIds lists stored outputs that are not fetched yet', () => {
  const seg = segments({
    steps: [
      {
        thinking: '',
        text: 'checking',
        toolCalls: [
          { name: 'shell', id: 'c1', storedOutput: { messageId: 11, bytes: 4 } },
          { name: 'shell', id: 'c2', output: 'live', storedOutput: { messageId: 12, bytes: 4 } },
          { name: 'shell', id: 'c3', storedOutput: { messageId: 13, bytes: 4, failed: true } },
          { name: 'shell', id: 'c4' },
        ],
      },
      { thinking: '', text: '', toolCalls: [{ name: 'shell', storedOutput: { messageId: 14, bytes: 1 } }] },
    ],
  });
  assert.deepEqual(pendingOutputIds(seg), [11, 13, 14]);
});

test('grouping does not mutate the input list', () => {
  const messages = [
    msg({ id: 't1', toolCall: { name: 'shell', id: 'c1' } }),
    msg({ id: 'a1', content: 'one', segments: segments({ steps: [toolStep('c1')] }) }),
  ];
  const snapshot = JSON.stringify(messages);
  groupMessages(messages);
  assert.equal(JSON.stringify(messages), snapshot);
});
