import type { WsMessage } from '../types/api';
import { contextLimitsFromFrame, type ContextLimitsState } from './contextLimits.logic.ts';

/** Provenance of the context meter's numerator, mirroring the wire literals
 * (`provider` / `estimate` / `calibrated`) so the ring panel can show exactly
 * what the daemon published. `null` when the meter has no number. */
export type ContextInputSource = 'provider' | 'estimate' | 'calibrated' | null;

export function contextSourceFromWire(source: string | undefined): ContextInputSource {
  if (source === 'provider' || source === 'calibrated') return source;
  // A frame with no label — `estimate` or an unlabelled sender — publishes an
  // estimate. The meter never claims `provider` for a number no provider sent.
  return 'estimate';
}

/** What one frame does to the ring. Omitted fields keep their current value. */
export interface ContextMeterStep {
  /** `null` clears the numerator: the accepted route reported no usage, and a
   * stale figure from a previous route is worse than none. */
  used?: number | null;
  source?: ContextInputSource;
  /** The serving route's window and budget, when the frame carries them. */
  limits?: ContextLimitsState;
}

/**
 * Fold one frame into the context meter — numerator, label and route snapshot.
 *
 * Four frames price the request, and the order they arrive in is the contract:
 *
 * - `history_trimmed` — the history just got smaller. It lands *before* the
 *   request goes out, so the ring must show the trimmed size immediately rather
 *   than the number that triggered the trim. The value is a fresh local estimate
 *   over the rebuilt transcript (the provider's anchor priced rows the trim
 *   dropped), hence the `estimate` label.
 * - `context_usage` — the assembled size of the request about to be dispatched:
 *   what makes the ring move at turn start, before any provider reply.
 * - `usage` — a call completed and the provider reported its real input size.
 * - `done` — the size the turn ended with; it wins over everything above except
 *   a later measurement.
 *
 * The route snapshot (`max_context_tokens` / `model_context_window` /
 * `effective_context_window`) rides on all four. A turn can switch models
 * mid-flight and keep running, so a route change has to be visible from the
 * first post-switch frame, not only from the terminal one; before this fold the
 * browser kept the previous route's `limit` and `trim budget` for the rest of
 * that turn while the runtime trimmed against the new route.
 */
export function contextMeterStep(msg: WsMessage): ContextMeterStep | null {
  const limits = contextLimitsFromFrame(msg);
  const limitsStep =
    limits.effectiveWindow !== null || limits.trimBudgetTokens !== null ? { limits } : {};

  switch (msg.type) {
    case 'history_trimmed': {
      if (typeof msg.tokens_after !== 'number') return null;
      return { used: msg.tokens_after, source: 'estimate', ...limitsStep };
    }

    case 'context_usage': {
      if (typeof msg.input_tokens !== 'number') return null;
      return {
        used: msg.input_tokens,
        source: contextSourceFromWire(msg.input_tokens_source),
        ...limitsStep,
      };
    }

    case 'usage': {
      if (typeof msg.input_tokens !== 'number') return null;
      return { used: msg.input_tokens, source: 'provider', ...limitsStep };
    }

    case 'done': {
      const step: ContextMeterStep = { ...limitsStep };
      // An explicit `null` means the accepted route published no usage: clear
      // the numerator instead of resurrecting an older route's figure.
      if (msg.last_input_tokens === null) step.used = null;
      else if (typeof msg.last_input_tokens === 'number') step.used = msg.last_input_tokens;
      else if (typeof msg.input_tokens === 'number') step.used = msg.input_tokens;
      // No numerator information at all and no route fields: nothing moved.
      if (step.used === undefined && step.limits === undefined) return null;
      return step;
    }

    default:
      return null;
  }
}
