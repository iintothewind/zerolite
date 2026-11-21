// Reasoning-effort picker: wire parsing and the choice held for this page.

/** Display order: `auto` (the default) first, then weakest to strongest. */
export const EFFORT_LEVELS = ['auto', 'off', 'low', 'medium', 'high', 'xhigh', 'max'] as const;
export type EffortLevel = typeof EFFORT_LEVELS[number];

export interface ReasoningInfo {
  /** Identifies the model the levels belong to. */
  key: string;
  levels: EffortLevel[];
  /** The endpoint refused the effort of the turn that just ended. */
  rejected: boolean;
}

export const EMPTY_REASONING: ReasoningInfo = { key: '', levels: [], rejected: false };

/** Wire → typed: drops unknown strings, keeps EFFORT_LEVELS order, dedups. */
export function parseReasoningInfo(raw: unknown): ReasoningInfo {
  if (typeof raw !== 'object' || raw === null) return EMPTY_REASONING;
  const wire = raw as { key?: unknown; levels?: unknown; rejected?: unknown };
  if (!Array.isArray(wire.levels)) return EMPTY_REASONING;
  const wireLevels: unknown[] = wire.levels;
  const levels = EFFORT_LEVELS.filter((level) => wireLevels.includes(level));
  if (levels.length === 0) return EMPTY_REASONING;
  return {
    key: typeof wire.key === 'string' ? wire.key : '',
    levels,
    rejected: wire.rejected === true,
  };
}

/** The levels the serving model offers and the one the next message is sent
 *  with. `auto` sends no effort: the configured behavior applies. */
export interface EffortState {
  info: ReasoningInfo;
  effort: EffortLevel;
}

export const INITIAL_EFFORT_STATE: EffortState = { info: EMPTY_REASONING, effort: 'auto' };

/**
 * The gateway published levels. The choice survives only for the same model,
 * while that model still offers it, and unless the endpoint just refused it;
 * otherwise it falls back to `auto`. Every level stays on offer after a
 * refusal: the user may pick again.
 */
export function publishReasoning(state: EffortState, info: ReasoningInfo): EffortState {
  const keep =
    !info.rejected && info.key === state.info.key && info.levels.includes(state.effort);
  return { info, effort: keep ? state.effort : 'auto' };
}

export function chooseEffort(state: EffortState, level: EffortLevel): EffortState {
  return state.info.levels.includes(level) ? { ...state, effort: level } : state;
}
