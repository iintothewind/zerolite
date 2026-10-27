import type { WsMessage } from '../types/api';

/**
 * The context limits of the route that served the last call, in the terms of
 * `docs/design/context-management.md`:
 *
 * - `effectiveWindow` — the **effective context window**
 *   (`model_window.min(request_token_ceiling)`). This is the meter
 *   denominator: the population a request is actually measured against, and
 *   the ceiling the model side enforces.
 * - `trimBudgetTokens` — the **trim budget**: the token count at which a trim
 *   engages (`min(window x trim_threshold_percent/100, window -
 *   reserve_tokens)`), and the size a trim retains to. It is a trigger, NOT a
 *   limit.
 * - `modelWindow` — the model's own capacity, before the ceiling is applied.
 */
export interface ContextLimitsState {
  effectiveWindow: number | null;
  trimBudgetTokens: number | null;
  modelWindow: number | null;
}

export const EMPTY_CONTEXT_LIMITS: ContextLimitsState = {
  effectiveWindow: null,
  trimBudgetTokens: null,
  modelWindow: null,
};

/**
 * Read a frame that carries the serving route as one authoritative
 * window/trim-budget snapshot.
 *
 * `done` and (since the mid-turn model switch) `usage` both publish all three
 * fields; `context_usage` publishes them too. A frame without them yields all
 * nulls, which callers read as "no route information here, keep what you have".
 */
export function contextLimitsFromFrame(
  frame: Pick<
    WsMessage,
    'max_context_tokens' | 'model_context_window' | 'effective_context_window'
  >,
): ContextLimitsState {
  const trimBudgetTokens =
    typeof frame.max_context_tokens === 'number' ? frame.max_context_tokens : null;
  const modelWindow =
    typeof frame.model_context_window === 'number' ? frame.model_context_window : null;
  const effectiveWindow =
    typeof frame.effective_context_window === 'number'
      ? frame.effective_context_window
      : // Older gateways did not publish the clamped window; the model's own
        // capacity is the closest thing to it.
        modelWindow ?? trimBudgetTokens;
  return { effectiveWindow, trimBudgetTokens, modelWindow };
}
