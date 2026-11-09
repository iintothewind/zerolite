// Pure projection from the message list to the blocks the chat renders.
//
// A turn arrives as a run of loose pieces: the user's prompt, narration
// bubbles, one standalone tool card per call, and usually a final answer.
// Rendering that as-is reads as "text, then N cards", which loses the order the
// steps happened in. This module folds every turn — the agent messages between
// two user messages — into one block: tool cards never render loose.
//
// The trajectory comes from `turnStream.logic`'s step segments when the turn
// was watched live (they ride on the committed message). A reloaded turn has
// no segments, so its trajectory is derived from the bubbles themselves: a text
// bubble is a step's narration, the cards after it are that step's calls, and
// a trailing text bubble with no calls after it is the answer.
//
// Pure and React-free so the grouping rules are pinned without mounting a
// component, matching the repo's `.logic.ts` convention.

import { stripServerTimestamp } from './stripServerTimestamp.ts';
import { emptyStep, type LiveTurn, type StepSegment, type TurnSegments } from './turnSegments.ts';
import type { ToolCall } from './toolCall.ts';

/** The message fields this projection reads. Structural rather than the
 *  context's `ChatMessage` so this module stays free of React. */
export interface FlowMessage {
  id: string;
  role: 'user' | 'agent';
  content: string;
  thinking?: string;
  markdown?: boolean;
  toolCall?: ToolCall;
  timestamp: Date;
  local?: boolean;
  ephemeral?: boolean;
  notice?: boolean;
  /** The turn's live trajectory, attached when the turn ends. Absent on every
   *  hydrated message: a reloaded turn's group is derived from its bubbles. */
  segments?: TurnSegments;
}

export type RenderBlock =
  | { kind: 'user'; message: FlowMessage }
  | { kind: 'notice'; message: FlowMessage }
  | { kind: 'plain'; message: FlowMessage }
  | { kind: 'turn'; message: FlowMessage; segments: TurnSegments }
  // A turn with tool calls and no answer bubble (stopped, failed, or ended on a
  // tool call). `id` is the first message of the run, for React keys and the
  // expanded-group map.
  | { kind: 'group'; id: string; segments: TurnSegments };

/** Whether a turn block is worth producing: a group with no steps would render
 *  an empty header reading `0 次工具调用`. */
function hasTrajectory(segments: TurnSegments | undefined): segments is TurnSegments {
  return Boolean(segments && segments.steps.length > 0);
}

/**
 * Group the message list into render blocks.
 *
 * A turn is the agent messages between two user messages. Its tool cards, its
 * narration bubbles and its trajectory fold into one block: a `turn` block
 * when it has an answer bubble, a `group` block when it does not. A message
 * carrying a live trajectory closes its turn — what follows it (an error
 * bubble, a cron result) starts a new run. Notices and ephemeral local
 * messages render in place and neither split nor join the run. A run with no
 * tool calls renders its bubbles plain, as no group would have anything in it.
 *
 * `showToolActivity: false` suppresses tool activity entirely, which means the
 * group too — its whole body is tool activity. The turn's answer survives as a
 * plain block.
 *
 * `liveSteps` is the in-flight turn's closed steps, which the transcript
 * renders as its own group below these blocks. Their calls are already in that
 * group, so the matching loose cards are dropped here rather than rendered
 * twice for the duration of the turn.
 */
export function groupMessages(
  messages: readonly FlowMessage[],
  options: {
    showToolActivity?: boolean;
    liveSteps?: readonly StepSegment[];
  } = {},
): RenderBlock[] {
  const showToolActivity = options.showToolActivity !== false;
  const blocks: RenderBlock[] = [];
  let run: FlowMessage[] = [];
  // Ids the in-flight group already carries. Ids are the gateway's
  // `tool_call_id`s, which the loose cards are keyed by too.
  const liveCallIds = callIds(options.liveSteps ?? []);

  const flushRun = () => {
    if (run.length === 0) return;
    blocks.push(...turnBlocks(run, showToolActivity));
    run = [];
  };

  for (const message of messages) {
    if (message.role === 'user') {
      flushRun();
      blocks.push({ kind: 'user', message });
    } else if (message.notice) {
      // A notice is never tool activity even if it carries a tool payload.
      blocks.push({ kind: 'notice', message });
    } else if (message.ephemeral) {
      blocks.push({ kind: 'plain', message });
    } else if (message.toolCall) {
      const id = message.toolCall.id;
      if (!(id && liveCallIds.has(id))) run.push(message);
    } else {
      run.push(message);
      if (hasTrajectory(message.segments)) flushRun();
    }
  }

  flushRun();
  return blocks;
}

function callIds(steps: readonly StepSegment[]): Set<string> {
  return new Set(
    steps
      .flatMap((step) => step.toolCalls.map((call) => call.id))
      .filter((id): id is string => Boolean(id)),
  );
}

/** Render one turn's run of agent messages. The run ends with the message
 *  carrying the live trajectory when there is one. */
function turnBlocks(run: readonly FlowMessage[], showToolActivity: boolean): RenderBlock[] {
  const last = run[run.length - 1];
  const live = last && hasTrajectory(last.segments) ? last : null;
  let steps: StepSegment[];
  let answer: FlowMessage | null;
  // The messages the block stands for; the first one keys the group.
  let members: readonly FlowMessage[];
  if (live) {
    // The trajectory supersedes the loose cards of the same calls.
    const carried = callIds(live.segments!.steps);
    const body = run
      .slice(0, -1)
      .filter((m) => !(m.toolCall?.id && carried.has(m.toolCall.id)));
    steps = [...deriveSteps(body, true).steps, ...live.segments!.steps];
    // Empty content and no thinking: a trajectory carrier, rendered as a group.
    answer = live.content.trim() || live.thinking ? live : null;
    members = [...body, live];
  } else {
    const derived = deriveSteps(run, false);
    steps = derived.steps;
    answer = derived.answer;
    members = run;
  }

  const calls = steps.reduce((n, step) => n + step.toolCalls.length, 0);
  if (!live && calls === 0) return run.map((message) => ({ kind: 'plain', message }));
  if (!showToolActivity) return answer ? [{ kind: 'plain', message: answer }] : [];
  if (answer) return [{ kind: 'turn', message: answer, segments: { steps } }];
  return [{ kind: 'group', id: members[0]!.id, segments: { steps } }];
}

/** Rebuild a trajectory from bubbles: a text bubble opens a step, the cards
 *  after it are that step's calls. With `closeAll` false, a trailing step with
 *  no calls is the answer and its message is returned instead of pushed. */
function deriveSteps(
  body: readonly FlowMessage[],
  closeAll: boolean,
): { steps: StepSegment[]; answer: FlowMessage | null } {
  const steps: StepSegment[] = [];
  let current: StepSegment | null = null;
  let currentMessage: FlowMessage | null = null;
  for (const m of body) {
    if (m.toolCall) {
      if (!current) current = emptyStep();
      current.toolCalls.push(m.toolCall);
      continue;
    }
    if (current) steps.push(current);
    current = {
      thinking: m.thinking ?? '',
      text: m.local || m.ephemeral ? m.content : stripServerTimestamp(m.content),
      toolCalls: [],
    };
    currentMessage = m;
  }
  if (!closeAll && current && current.toolCalls.length === 0 && currentMessage) {
    return { steps, answer: currentMessage };
  }
  if (current) steps.push(current);
  return { steps, answer: null };
}

/**
 * Split the in-flight turn the way the committed one is split: everything that
 * cannot be the answer goes in the group, and the answer still arriving sits
 * below it in the bubble.
 *
 * The answer is the step being written, or — once nothing is being written —
 * the last closed step, which is the rule `finalizeSegments` applies when the
 * turn ends. Two consequences the view depends on: a tool-free turn never grows
 * a group on its way through, and text only moves once, when the turn continues
 * past it and it stops being the answer.
 */
export interface LiveTurnSplit {
  /** Steps that cannot be the answer: they belong in the group. */
  groupSteps: StepSegment[];
  /** The answer still arriving, or `null` while the model is between steps. */
  answer: StepSegment | null;
}

export function splitLiveTurn(live: LiveTurn): LiveTurnSplit {
  if (live.open.text || live.open.thinking) {
    return { groupSteps: live.steps, answer: live.open };
  }
  const last = live.steps[live.steps.length - 1];
  if (last && last.toolCalls.length === 0) {
    return { groupSteps: live.steps.slice(0, -1), answer: last };
  }
  // Nothing open and the last step made a call: the turn ended on a tool call,
  // so there is no answer text — its cards are the record.
  return { groupSteps: live.steps, answer: null };
}

/** How many tool calls a turn made — the reference header's first number. */
export function countToolCalls(segments: TurnSegments): number {
  return segments.steps.reduce((total, step) => total + step.toolCalls.length, 0);
}

/** How many messages a turn's group holds — the reference header's second
 *  number: the steps that carry thinking or text, i.e. the visible non-tool
 *  items inside the group. The reference does not document this, so if it turns
 *  out to mean something else, only this function changes. */
export function countGroupMessages(segments: TurnSegments): number {
  return segments.steps.filter((step) => step.thinking.trim() || step.text.trim()).length;
}
