import type { WsMessage } from '../types/api';

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

/**
 * The meter movement a frame implies, or `null` when the frame does not touch
 * the meter.
 *
 * `used: null` clears the numerator: the accepted route reported no usage, and
 * a stale figure from a previous route is worse than none.
 */
export interface MeterUpdate {
  used: number | null;
  /** New provenance label; `undefined` keeps the current one. */
  source?: ContextInputSource;
}

/**
 * Fold one frame into the context meter.
 *
 * Four frames move it, and the order they arrive in is the contract:
 *
 * - `history_trimmed` — the history just got smaller. It lands *before* the
 *   request goes out, so the ring must show the trimmed size immediately
 *   rather than the number that triggered the trim. The value is a fresh local
 *   estimate over the rebuilt transcript (the provider's anchor priced rows the
 *   trim dropped), hence the `estimate` label.
 * - `context_usage` — the assembled size of the request about to be dispatched.
 *   This is what makes the ring move at turn start, before any provider reply.
 * - `usage` — a call completed and the provider reported its real input size.
 * - `done` — the size the turn ended with; it wins over everything above except
 *   a later measurement.
 */
export function meterUpdateFromFrame(msg: WsMessage): MeterUpdate | null {
  switch (msg.type) {
    case 'history_trimmed':
      return typeof msg.tokens_after === 'number'
        ? { used: msg.tokens_after, source: 'estimate' }
        : null;

    case 'context_usage':
      return typeof msg.input_tokens === 'number'
        ? { used: msg.input_tokens, source: contextSourceFromWire(msg.input_tokens_source) }
        : null;

    case 'usage':
      return typeof msg.input_tokens === 'number'
        ? { used: msg.input_tokens, source: 'provider' }
        : null;

    case 'done':
      // An explicit `null` means the accepted route published no usage: clear
      // the numerator instead of resurrecting an older route's figure.
      if (msg.last_input_tokens === null) return { used: null };
      if (typeof msg.last_input_tokens === 'number') return { used: msg.last_input_tokens };
      return typeof msg.input_tokens === 'number' ? { used: msg.input_tokens } : null;

    default:
      return null;
  }
}
