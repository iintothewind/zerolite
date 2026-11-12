// Pure completion/turn-stream reducer used by AgentContext's WebSocket handler
// so the production lifecycle is testable without mounting React. This file
// makes no DOM/i18n/UUID side effects; it owns only per-turn stream state,
// completion classification, and the turn's step trajectory. The trajectory's
// *shape* (`StepSegment` / `TurnSegments` / `LiveTurn`) is shared with the
// message-flow projection and lives in `lib/turnSegments.ts`.
//
// One store, two readers. The trajectory (`segments` + `openStep`) is the only
// place streamed text and reasoning are kept; the flat buffers
// `classifyCompletion` reads are derived from it on demand. Keeping a second
// copy of the same deltas is what let the two drift apart.

import { resolveToolResultLocation } from '../lib/toolCardMatch.ts';
import type { ToolCall } from '../lib/toolCall.ts';
import {
  emptySegments,
  emptyStep,
  type StepSegment,
  type TurnSegments,
} from '../lib/turnSegments.ts';

/** The slice of turn state `classifyCompletion` reads. Declared on its own so a
 *  caller can classify a finished turn without materialising a full
 *  `TurnStreamState` (the trajectory's shape is irrelevant to the decision). */
export interface CompletionInput {
  /** Everything streamed this turn, every step joined. */
  streamedContent: string;
  /** Every reasoning delta streamed this turn, every step joined. */
  streamedThinking: string;
  hadToolCall: boolean;
}

/** Accumulated per-turn streaming state the reducer folds frames into. */
export interface TurnStreamState {
  /** Closed steps, in the order they happened. */
  segments: TurnSegments;
  /** The step currently being written. `thinking` / `chunk` land here until the
   *  `usage` frame closes it. */
  openStep: StepSegment;
  /** A named `tool_call` frame arrived this turn. Set even when the frame
   *  carries no call payload, so it is not derivable from the trajectory. */
  hadToolCall: boolean;
}

/** Fresh per-turn state. Returned after every completion so the next turn
 *  starts clean — the "ref state resets across turns" invariant. */
export function initialTurnStreamState(): TurnStreamState {
  return { segments: emptySegments(), openStep: emptyStep(), hadToolCall: false };
}

/** Everything streamed as visible text this turn. */
export function streamedText(state: TurnStreamState): string {
  return state.segments.steps.map((step) => step.text).join('') + state.openStep.text;
}

function streamedThinking(state: TurnStreamState): string {
  return state.segments.steps.map((step) => step.thinking).join('') + state.openStep.thinking;
}

function completionInput(state: TurnStreamState): CompletionInput {
  return {
    streamedContent: streamedText(state),
    streamedThinking: streamedThinking(state),
    hadToolCall: state.hadToolCall,
  };
}

/** Events that affect the current turn's accumulated stream state. The handler
 *  maps gateway frames and local lifecycle boundaries to these before folding;
 *  events the completion decision does not depend on are not modeled. */
export type TurnStreamFrame =
  | { type: 'thinking'; content?: string }
  | { type: 'chunk'; content?: string }
  | { type: 'chunk_reset' }
  // `hasName` mirrors the handler's `if (!msg.name) break` guard: a nameless
  // observability telemetry frame is not a real tool call and must not flip
  // `hadToolCall` (issue #7151). `call` carries the payload for the step
  // trajectory and is absent on such frames.
  | { type: 'tool_call'; hasName: boolean; call?: ToolCall }
  | { type: 'tool_result'; id?: string; output: string }
  // One LLM call completed. This is the *only* server-authoritative step
  // boundary: it arrives once the response is accepted, before that step's
  // tool-call events.
  | { type: 'usage' }
  | { type: 'done' | 'message'; full_response?: string; content?: string }
  // Every terminal/non-completion boundary resets the same canonical state.
  // `turn_start` is emitted locally after a new message is sent.
  | { type: 'turn_start' | 'aborted' | 'error' | 'reset' };

/** What the handler should do with the finished turn. `commit` renders an
 *  assistant bubble (possibly empty content when reasoning is present);
 *  `diagnostic` renders the one-off no-output notice; `skip` renders nothing
 *  (the tool cards are already the visible record — #6702). */
export type CompletionOutcome =
  | { kind: 'commit'; content: string; thinking?: string }
  | { kind: 'diagnostic' }
  | { kind: 'skip' };

/** Classify a finished turn from its accumulated state and the terminal frame.
 *  Pure: the whole reasoning-only / empty / tool-only decision lives here. */
export function classifyCompletion(
  state: CompletionInput,
  frame: { full_response?: string; content?: string },
): CompletionOutcome {
  // Fallback chain matches the handler: an explicit `full_response`, then a
  // frame `content`, then whatever was streamed live.
  const raw = frame.full_response ?? frame.content ?? state.streamedContent;
  // Trim so whitespace-only content (models that emit "\n\n" alongside
  // tool_calls) does not create a blank bubble (#6702).
  const content = raw.trim();
  const thinking = state.streamedThinking || undefined;

  if (content || thinking) {
    // Reasoning-only turns land here with empty content but present thinking,
    // so the turn is visible instead of vanishing silently.
    return { kind: 'commit', content, thinking };
  }
  if (!state.hadToolCall) {
    // Nothing at all on a clean completion — surface a diagnostic so the turn
    // does not disappear. Mirrors zerocode's zc-turn-no-output (#8779).
    return { kind: 'diagnostic' };
  }
  // Empty content but tools ran: their cards are the record, render nothing.
  return { kind: 'skip' };
}

function stepHasContent(step: StepSegment): boolean {
  return Boolean(step.thinking || step.text || step.toolCalls.length);
}

/** Close the step being written, pushing it onto the trajectory only if it
 *  carries something. An empty step is not a step. */
export function closeStep(state: TurnStreamState): TurnStreamState {
  if (!stepHasContent(state.openStep)) return state;
  return {
    ...state,
    segments: { ...state.segments, steps: [...state.segments.steps, state.openStep] },
    openStep: emptyStep(),
  };
}

/** Attach a tool call to the step that made it.
 *
 *  The `usage` frame closes a step as soon as its response is accepted, and
 *  the tool-call events for that step arrive *after* it — so a call lands on
 *  the last closed step, and several calls landing there is parallel tool use
 *  within one step. A call with no step to attach to (no preceding thinking or
 *  text) opens a step of its own, because the tools are the record. */
export function attachToolCall(
  state: TurnStreamState,
  call: ToolCall,
): TurnStreamState {
  const closed = closeStep(state);
  const steps =
    closed.segments.steps.length > 0
      ? closed.segments.steps
      : [emptyStep()];
  const last = steps[steps.length - 1]!;
  const nextSteps = [
    ...steps.slice(0, -1),
    { ...last, toolCalls: [...last.toolCalls, call] },
  ];
  return { ...closed, segments: { ...closed.segments, steps: nextSteps } };
}

/** Fill the matching call's output, correlating by gateway `tool_call_id` the
 *  same way the loose tool cards do. */
export function attachToolResult(
  state: TurnStreamState,
  id: string | undefined,
  output: string,
): TurnStreamState {
  const found = resolveToolResultLocation(state.segments.steps, id);
  if (!found) return state;
  const { stepIndex, callIndex } = found;
  const step = state.segments.steps[stepIndex]!;
  const toolCalls = [...step.toolCalls];
  toolCalls[callIndex] = { ...toolCalls[callIndex]!, output };
  const steps = [...state.segments.steps];
  steps[stepIndex] = { ...step, toolCalls };
  return { ...state, segments: { ...state.segments, steps } };
}

/** Split the accumulated trajectory into the finished shape: the step that
 *  produced the final answer leaves `steps` as `answer`, because it renders as
 *  the committed bubble rather than inside the group.
 *
 *  Only a step with no tool calls can be the final answer — a turn that ends
 *  on a tool call has no answer text, and its step stays in the trajectory. */
export function splitFinishedTurn(state: TurnStreamState): {
  segments: TurnSegments;
  answer: StepSegment | null;
} {
  const closed = closeStep(state);
  const steps = [...closed.segments.steps];
  const last = steps[steps.length - 1];
  const answer = last && last.toolCalls.length === 0 ? steps.pop()! : null;
  return { segments: { steps }, answer };
}

function finalizeSegments(state: TurnStreamState): TurnSegments {
  return splitFinishedTurn(state).segments;
}

/** Fold one frame into the turn state. For `done`/`message` the returned
 *  `completion` is the classification and the returned `state` is fresh (the
 *  turn is over); for every other frame `completion` is null and `state`
 *  carries the accumulation forward.
 *
 *  `result.segments` is the trajectory as of *after* the fold: for a terminal
 *  frame it is the finished shape (read it before the state resets), and for
 *  every other frame it is the trajectory so far. */
export function reduceTurnFrame(
  state: TurnStreamState,
  frame: TurnStreamFrame,
): { state: TurnStreamState; completion: CompletionOutcome | null; segments: TurnSegments } {
  switch (frame.type) {
    case 'thinking':
      return {
        state: {
          ...state,
          openStep: { ...state.openStep, thinking: state.openStep.thinking + (frame.content ?? '') },
        },
        completion: null,
        segments: state.segments,
      };
    case 'chunk':
      return {
        state: {
          ...state,
          openStep: { ...state.openStep, text: state.openStep.text + (frame.content ?? '') },
        },
        completion: null,
        segments: state.segments,
      };
    case 'chunk_reset':
      // The server is restarting the content stream and will resend the
      // authoritative response with the terminal frame, so the text written so
      // far is discarded. Reasoning is not resent, so it stays on the step.
      return {
        state: { ...state, openStep: { ...state.openStep, text: '' } },
        completion: null,
        segments: state.segments,
      };
    case 'usage': {
      const closed = closeStep(state);
      return { state: closed, completion: null, segments: closed.segments };
    }
    case 'tool_call': {
      if (!frame.hasName) return { state, completion: null, segments: state.segments };
      const withCall = frame.call ? attachToolCall(state, frame.call) : state;
      return {
        state: { ...withCall, hadToolCall: true },
        completion: null,
        segments: withCall.segments,
      };
    }
    case 'tool_result': {
      const resolved = attachToolResult(state, frame.id, frame.output);
      return { state: resolved, completion: null, segments: resolved.segments };
    }
    case 'done':
    case 'message': {
      const completion = classifyCompletion(completionInput(state), frame);
      const segments = finalizeSegments(state);
      // Turn is over: hand back fresh state so the next turn starts clean.
      return { state: initialTurnStreamState(), completion, segments };
    }
    case 'aborted': {
      // A stopped turn keeps what it streamed: the step being written when
      // the user stopped is the partial answer, everything before it the group.
      // `closeStep` never pushes an empty step, so an answer has text or thinking.
      const { segments, answer } = splitFinishedTurn(state);
      return {
        state: initialTurnStreamState(),
        completion: answer
          ? { kind: 'commit', content: answer.text.trim(), thinking: answer.thinking || undefined }
          : null,
        segments,
      };
    }
    case 'error': {
      // A failed turn has no answer: the partial text stays inside the group
      // as its last step.
      const closed = closeStep(state);
      return { state: initialTurnStreamState(), completion: null, segments: closed.segments };
    }
    case 'turn_start':
    case 'reset': {
      // Keep whatever was accumulated for the caller to inspect, but the next
      // turn starts clean.
      const segments = finalizeSegments(state);
      return { state: initialTurnStreamState(), completion: null, segments };
    }
  }
}

/**
 * Whether an `error` frame ends the running turn. Two codes refuse only the
 * request that caused them, so the turn keeps streaming: `CAPABILITY_MISSING`
 * (a message sent mid-turn, or an SOP approval answer, on a gateway that
 * serves chat through the core) and `INVALID_JSON` (a malformed client frame).
 * Every other error code ends it.
 */
export function errorEndsTurn(code: string | undefined): boolean {
  return code !== 'CAPABILITY_MISSING' && code !== 'INVALID_JSON';
}
