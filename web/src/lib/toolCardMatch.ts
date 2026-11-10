import { isPendingCall, type ToolCall } from './toolCall.ts';

/** The fields a `tool_result` frame is matched against, on either carrier: a
 *  loose tool card or a call inside a step. Derived from {@link ToolCall} so
 *  `output`/`id` stay single-sourced. */
export type ToolCallLike = Pick<ToolCall, 'output' | 'id' | 'storedOutput'>;

interface ToolCardLike {
  toolCall?: ToolCallLike;
}

interface StepLike {
  toolCalls: readonly ToolCallLike[];
}

/** Resolve which pending tool card a `tool_result` frame belongs to.
 *
 * Parallel tool batches can complete out of call order, so a result must be
 * correlated to its card by the gateway `tool_call_id` rather than by position.
 * When the frame carries no id (legacy/non-gateway paths) or no id-keyed
 * pending card exists, fall back to the first unresolved card so id-less
 * streams still resolve. A card whose output is stored on the server is
 * resolved. Returns -1 when nothing is pending. */
export function resolveToolResultIndex<T extends ToolCardLike>(
  messages: readonly T[],
  resultId: string | undefined,
): number {
  const firstUnresolved = () =>
    messages.findIndex((m) => m.toolCall && isPendingCall(m.toolCall));
  if (!resultId) return firstUnresolved();
  const byId = messages.findIndex(
    (m) => m.toolCall && isPendingCall(m.toolCall) && m.toolCall.id === resultId,
  );
  return byId === -1 ? firstUnresolved() : byId;
}

/** Where a `tool_result` frame belongs inside a step trajectory: which step,
 *  and which call within it. The matching rule is `resolveToolResultIndex`'s —
 *  this only maps its flat answer back onto the nesting. Returns null when
 *  nothing is pending. */
export function resolveToolResultLocation(
  steps: readonly StepLike[],
  resultId: string | undefined,
): { stepIndex: number; callIndex: number } | null {
  const flat: Array<{ stepIndex: number; callIndex: number; toolCall: ToolCallLike }> = [];
  steps.forEach((step, stepIndex) => {
    step.toolCalls.forEach((toolCall, callIndex) => {
      flat.push({ stepIndex, callIndex, toolCall });
    });
  });
  const index = resolveToolResultIndex(flat, resultId);
  if (index === -1) return null;
  const { stepIndex, callIndex } = flat[index]!;
  return { stepIndex, callIndex };
}
