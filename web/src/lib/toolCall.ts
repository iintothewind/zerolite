/** The one shape for a tool invocation, shared by every carrier that moves one
 *  around: a step segment, a persisted bubble, a live flow message, and the
 *  card component all mean exactly this. Declared in a pure module (no React,
 *  no path aliases) so any `.logic.ts` file can import it for the Node test
 *  runner without mounting a component. */
export interface ToolCall {
  name: string;
  args?: unknown;
  /** The runtime's result. `undefined` while the call is still executing;
   *  a `string` once reported. */
  output?: string;
  /** Gateway `tool_call_id` — correlates a later `tool_result` frame back to
   *  this call. */
  id?: string;
  /** Output stored on the server but not fetched yet: the row holding it.
   *  Only hydrated cards have it; `output` stays undefined until fetched. */
  storedOutput?: { messageId: number; bytes: number; failed?: boolean };
}

/** Whether the call is still waiting for its result. A call whose output is
 *  stored on the server is finished, even before that output is fetched. */
export function isPendingCall(call: Pick<ToolCall, 'output' | 'storedOutput'>): boolean {
  return call.output === undefined && call.storedOutput === undefined;
}
