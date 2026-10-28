/**
 * Live generation throughput meter. Ported verbatim from omp's
 * `packages/coding-agent/src/utils/token-rate.ts` (`TokenRateMeter`),
 * adapted to zerolite's TS + web frames. Constants and logic unchanged.
 */

const METER_HALF_LIVES_MS = [5_000, 20_000, 80_000];
const METER_MIN_TOKENS = 200;
const METER_MIN_TIME_MS = 4_000;
const METER_BUCKET_MS = 250;
const METER_CARRY_MAX_CHARS = 32;
const METER_HIDDEN_DECAY = 0.8;
const METER_HIDDEN_PRIOR_MS = 10_000;
const LN2 = Math.LN2;

class DecayedSums {
  tokens = 0;
  time = 0;
  readonly halfLifeMs: number;

  constructor(halfLifeMs: number) {
    this.halfLifeMs = halfLifeMs;
  }

  advance(dtMs: number, inflight: boolean, hiddenRate: number): void {
    if (dtMs <= 0) return;
    const f = 2 ** (-dtMs / this.halfLifeMs);
    this.tokens *= f;
    this.time *= f;
    if (inflight) {
      const integral = (this.halfLifeMs / LN2) * (1 - f);
      this.time += integral;
      this.tokens += hiddenRate * integral;
    }
  }

  reset(): void {
    this.tokens = 0;
    this.time = 0;
  }
}

export class TokenRateMeter {
  readonly #count: (text: string) => number;
  readonly #history = METER_HALF_LIVES_MS.map((ms) => new DecayedSums(ms));
  readonly #inflight = METER_HALF_LIVES_MS.map((ms) => new DecayedSums(ms));
  #startedAt: number | null = null;
  #advancedTo = 0;
  #inflightLocal = 0;
  #inflightHiddenRate = 0;
  #pendingIndex = -1;
  #pending = '';
  #pendingTokens = -1;
  #hiddenTokens = 0;
  #hiddenSpanMs = 0;

  constructor(count: (text: string) => number) {
    this.#count = count;
  }

  begin(nowMs: number = Date.now()): void {
    this.#clearInflight();
    this.#startedAt = nowMs;
    this.#advancedTo = nowMs;
    this.#inflightHiddenRate = Math.max(0, this.#hiddenTokens / (this.#hiddenSpanMs + METER_HIDDEN_PRIOR_MS));
  }

  push(text: string, nowMs: number = Date.now()): void {
    if (text.length === 0) return;
    if (this.#startedAt === null) this.begin(nowMs);
    const index = Math.floor((nowMs - (this.#startedAt ?? nowMs)) / METER_BUCKET_MS);
    if (index !== this.#pendingIndex) {
      this.#flushPending(true, nowMs);
      this.#pendingIndex = index;
    }
    this.#pending += text;
    this.#pendingTokens = -1;
  }

  end(outputTokens: number | undefined, nowMs: number = Date.now()): void {
    if (this.#startedAt === null) return;
    this.#flushPending(false, nowMs);
    this.#advance(nowMs);
    const spanMs = nowMs - this.#startedAt;
    const billed = outputTokens !== undefined && Number.isFinite(outputTokens) && outputTokens > 0;
    let extra = 0;
    if (billed) {
      const hidden = outputTokens - this.#inflightLocal;
      this.#hiddenTokens = this.#hiddenTokens * METER_HIDDEN_DECAY + hidden;
      this.#hiddenSpanMs = this.#hiddenSpanMs * METER_HIDDEN_DECAY + spanMs;
      extra = hidden - this.#inflightHiddenRate * spanMs;
    }
    for (let k = 0; k < this.#history.length; k++) {
      const live = this.#inflight[k]!;
      const corrected = live.tokens + (spanMs > 0 ? (extra * live.time) / spanMs : 0);
      this.#history[k]!.tokens += Math.max(0, corrected);
      this.#history[k]!.time += live.time;
    }
    this.#clearInflight();
  }

  reset(): void {
    this.#clearInflight();
    for (const sums of this.#history) sums.reset();
  }

  seed(outputTokens: number, durationMs: number): void {
    if (!Number.isFinite(outputTokens) || outputTokens <= 0 || !Number.isFinite(durationMs) || durationMs <= 0) {
      this.reset();
      return;
    }
    this.#clearInflight();
    const scale = Math.max(1, METER_MIN_TOKENS / outputTokens, METER_MIN_TIME_MS / durationMs);
    for (const sums of this.#history) {
      sums.tokens = outputTokens * scale;
      sums.time = durationMs * scale;
    }
  }

  rate(nowMs: number = Date.now()): number | null {
    const dtMs = this.#startedAt === null ? 0 : nowMs - this.#advancedTo;
    const pendingTokens = this.#countPending();
    let tokens = 0;
    let time = 0;
    let evidenceTokens = 0;
    let evidenceTime = 0;
    for (let k = 0; k < this.#history.length; k++) {
      const f = 2 ** (-dtMs / METER_HALF_LIVES_MS[k]!);
      const integral = (METER_HALF_LIVES_MS[k]! / LN2) * (1 - f);
      evidenceTokens =
        (this.#history[k]!.tokens + this.#inflight[k]!.tokens) * f +
        this.#inflightHiddenRate * integral +
        pendingTokens;
      evidenceTime = (this.#history[k]!.time + this.#inflight[k]!.time) * f + integral;
      tokens += evidenceTokens;
      time += evidenceTime;
    }
    if (evidenceTokens < METER_MIN_TOKENS || evidenceTime < METER_MIN_TIME_MS) return null;
    return (tokens * 1000) / time;
  }

  #advance(nowMs: number): void {
    const dtMs = nowMs - this.#advancedTo;
    if (dtMs <= 0) return;
    this.#advancedTo = nowMs;
    for (let k = 0; k < this.#history.length; k++) {
      this.#history[k]!.advance(dtMs, false, 0);
      this.#inflight[k]!.advance(dtMs, true, this.#inflightHiddenRate);
    }
  }

  #clearInflight(): void {
    for (const sums of this.#inflight) sums.reset();
    this.#startedAt = null;
    this.#inflightLocal = 0;
    this.#inflightHiddenRate = 0;
    this.#pendingIndex = -1;
    this.#pending = '';
    this.#pendingTokens = -1;
  }

  #countPending(): number {
    if (this.#pending.length === 0) return 0;
    if (this.#pendingTokens < 0) this.#pendingTokens = this.#count(this.#pending);
    return this.#pendingTokens;
  }

  #flushPending(carry: boolean, nowMs: number): void {
    if (this.#pendingIndex < 0 || this.#pending.length === 0) return;
    let text = this.#pending;
    let tail = '';
    if (carry) {
      const cut = Math.max(text.lastIndexOf(' '), text.lastIndexOf('\n'));
      if (cut > 0 && text.length - cut <= METER_CARRY_MAX_CHARS) {
        tail = text.slice(cut);
        text = text.slice(0, cut);
      }
    }
    this.#pending = tail;
    this.#pendingTokens = -1;
    if (text.length === 0) return;
    this.#advance(nowMs);
    const tokens = this.#count(text);
    for (const sums of this.#inflight) sums.tokens += tokens;
    this.#inflightLocal += tokens;
  }
}

export function estimateTokens(text: string): number {
  if (!text) return 0;
  let tokens = 0;
  let nonWs = 0;
  let ws = 0;
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    const cjk =
      (code >= 0x4e00 && code <= 0x9fff) ||
      (code >= 0x3040 && code <= 0x30ff) ||
      (code >= 0xff00 && code <= 0xffef) ||
      (code >= 0x3400 && code <= 0x4dbf);
    if (/\s/.test(text[i]!)) {
      ws += 1;
    } else if (cjk) {
      tokens += 1.2;
    } else {
      nonWs += 1;
    }
  }
  tokens += nonWs / 4;
  // Whitespace feeds the ratio like any other character: a quarter token.
  tokens += ws / 4;
  return tokens;
}
