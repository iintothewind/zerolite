// Composer stats: turns/steps from the local transcript, cache hit from this
// page's `usage` frames, and the live rate from the meter snapshot the caller
// passes in. Canonical rule: `docs/design/webui.md` → "Composer stats semantics".

/** Wire payload of a per-step `usage` frame. */
export interface UsageFrame {
  input_tokens?: number | null;
  output_tokens?: number | null;
  cached_input_tokens?: number | null;
}

/** Wire payload of a turn-terminating frame (`done` / `aborted`). */
export interface TurnTerminalFrame {
  cached_input_tokens?: number | null;
}

export interface TranscriptMessage {
  role: 'user' | 'agent';
  toolCall?: unknown;
  ephemeral?: boolean;
  notice?: boolean;
  /** Absent after refresh — then only the final answer bubble remains. */
  segments?: { steps: readonly TranscriptLiveStep[] };
}

export interface TranscriptLiveStep {
  thinking: string;
  text: string;
  toolCalls: readonly unknown[];
}

export interface TranscriptLiveTurn {
  steps: readonly TranscriptLiveStep[];
  open: TranscriptLiveStep;
}

export interface LiveStats {
  turns: number;
  steps: number;
  input: number;
  output: number;
  cached: number;
  rate: number | null;
  rateMuted: boolean;
  rateBilled: boolean;
}

export interface TokenStats {
  input: number;
  output: number;
  cached: number;
  turnCached: number;
}

export const emptyTokenStats: TokenStats = {
  input: 0,
  output: 0,
  cached: 0,
  turnCached: 0,
};

function wireNumber(value: number | null | undefined): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : 0;
}

function isCountedBubble(message: TranscriptMessage): boolean {
  return !message.ephemeral && !message.notice;
}

function countFlowMessages(steps: readonly TranscriptLiveStep[]): number {
  let n = 0;
  for (const step of steps) {
    if (step.thinking.trim() || step.text.trim()) n += 1;
  }
  return n;
}

function countFlowToolCalls(steps: readonly TranscriptLiveStep[]): number {
  let n = 0;
  for (const step of steps) {
    n += step.toolCalls.length;
  }
  return n;
}

function countFlowUnits(
  steps: readonly TranscriptLiveStep[],
  finalAnswer: boolean,
): number {
  return countFlowMessages(steps) + countFlowToolCalls(steps) + (finalAnswer ? 1 : 0);
}

export function countTranscriptTurns(
  messages: readonly TranscriptMessage[],
): number {
  let n = 0;
  for (const message of messages) {
    if (message.role === 'user' && isCountedBubble(message)) n += 1;
  }
  return n;
}

export function countTranscriptSteps(
  messages: readonly TranscriptMessage[],
  liveTurn?: TranscriptLiveTurn | null,
): number {
  let n = 0;
  for (const message of messages) {
    if (message.role !== 'agent' || !isCountedBubble(message)) continue;
    if (message.toolCall !== undefined) continue;
    const prefix = message.segments?.steps ?? [];
    if (prefix.length > 0) {
      n += countFlowUnits(prefix, true);
    } else {
      n += 1;
    }
  }
  if (liveTurn) {
    const closed = liveTurn.steps;
    const open = liveTurn.open;
    n += countFlowMessages(closed) + countFlowToolCalls(closed);
    n += open.toolCalls.length;
    if (open.thinking.trim() || open.text.trim()) n += 1;
  }
  return n;
}

export function displayStats(
  messages: readonly TranscriptMessage[],
  tokens: TokenStats,
  liveTurn?: TranscriptLiveTurn | null,
  meterSnapshot?: { rate: number | null; rateMuted: boolean; rateBilled: boolean } | null,
): LiveStats {
  const snap = meterSnapshot ?? null;
  return {
    turns: countTranscriptTurns(messages),
    steps: countTranscriptSteps(messages, liveTurn),
    input: tokens.input,
    output: tokens.output,
    cached: tokens.cached,
    rate: snap ? snap.rate : null,
    rateMuted: snap ? snap.rateMuted : false,
    rateBilled: snap ? snap.rateBilled : false,
  };
}

export function turnStarted(s: TokenStats): TokenStats {
  return { ...s, turnCached: 0 };
}

export function applyUsage(s: TokenStats, frame: UsageFrame): TokenStats {
  const cached = wireNumber(frame.cached_input_tokens);
  return {
    ...s,
    input: s.input + wireNumber(frame.input_tokens),
    output: s.output + wireNumber(frame.output_tokens),
    cached: s.cached + cached,
    turnCached: s.turnCached + cached,
  };
}

export function applyDone(s: TokenStats, frame: TurnTerminalFrame): TokenStats {
  const turnCached =
    typeof frame.cached_input_tokens === 'number' &&
    Number.isFinite(frame.cached_input_tokens)
      ? frame.cached_input_tokens
      : s.turnCached;
  return {
    ...s,
    cached: s.cached + Math.max(0, turnCached - s.turnCached),
  };
}

/** `cached ÷ input`, clamped to `[0, 1]`. Zero input → `0`. */
export function cacheHitRatio(s: Pick<LiveStats, 'input' | 'cached'>): number {
  if (s.input <= 0) return 0;
  return Math.min(1, Math.max(0, s.cached / s.input));
}

/**
 * Rate readout: below `0.05` tok/s the meter is not worth a reading, so "0";
 * ≥10 reads as a whole number; below that, one decimal keeps the width stable
 * between ticks (`tabular-nums`).
 */
export function formatRate(rate: number): string {
  if (!Number.isFinite(rate) || rate < 0.05) return '0';
  if (rate >= 10) return String(Math.round(rate));
  return (Math.round(rate * 10) / 10).toFixed(1);
}

export function formatTokens(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '0';
  const scaled: [number, string][] = [
    [1e9, 'B'],
    [1e6, 'M'],
    [1e3, 'K'],
  ];
  for (const [unit, suffix] of scaled) {
    if (n >= unit) {
      const value = n / unit;
      const rounded = Math.round(value * 10) / 10;
      return `${Number.isInteger(rounded) ? rounded : rounded.toFixed(1)}${suffix}`;
    }
  }
  return String(Math.round(n));
}
