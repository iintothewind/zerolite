import assert from 'node:assert/strict';
import test from 'node:test';

import { TokenRateMeter, estimateTokens } from './tokenRate.ts';

/** Counts words, the count omp's meter tests use as the tokenizer. */
const words = (text: string) => text.split(/\s+/).filter(Boolean).length;

/** Stream `perDelta` words every 100ms from `from` to `to`. */
function stream(meter: TokenRateMeter, from: number, to: number, perDelta: number) {
  const delta = `${'w '.repeat(perDelta)}`;
  for (let t = from; t < to; t += 100) meter.push(delta, t);
}

/** `count` messages of 600 tokens over 10s at 60 tok/s, 5s tool gaps, billed as counted. Returns the clock. */
function settle(meter: TokenRateMeter, count: number, billed = 600): number {
  let clock = 0;
  for (let i = 0; i < count; i++) {
    meter.begin(clock);
    stream(meter, clock, clock + 10_000, 6);
    clock += 10_000;
    meter.end(billed, clock);
    clock += 5_000;
  }
  return clock;
}

/** `assert.closeTo` equivalent: `digits` decimal places, i.e. tolerance `0.5 × 10⁻ᵈ`. */
function closeTo(actual: number | null, expected: number, digits: number) {
  assert.notEqual(actual, null, `expected ${expected}, got null`);
  assert.ok(
    Math.abs((actual as number) - expected) < 0.5 * 10 ** -digits,
    `expected ${actual} to be close to ${expected} (${digits} digits)`,
  );
}

// ── Contracts ported from omp's token-rate-meter suite ──────────────────────

test('null until enough tokens and stream time have accumulated', () => {
  const meter = new TokenRateMeter(words);
  assert.equal(meter.rate(0), null);
  meter.begin(0);
  // A provider's first buffered chunk: 300 tokens in one delta is not a rate yet.
  meter.push('w '.repeat(300), 500);
  assert.equal(meter.rate(600), null);
  stream(meter, 600, 10_000, 6);
  const rate = meter.rate(10_000);
  assert.notEqual(rate, null);
  assert.ok(rate! > 80, `expected > 80, got ${rate}`);
  assert.ok(rate! < 100, `expected < 100, got ${rate}`);
});

test('holds across tool execution and is only nudged by a short burst', () => {
  const meter = new TokenRateMeter(words);
  const clock = settle(meter, 6);
  closeTo(meter.rate(clock), 60, 0);
  closeTo(meter.rate(clock + 60_000), 60, 0);
  // 2s write-args burst at 200 tok/s.
  meter.begin(clock);
  stream(meter, clock, clock + 2000, 20);
  const peak = meter.rate(clock + 2000) ?? 0;
  assert.ok(peak > 60, `expected a nudge above 60, got ${peak}`);
  assert.ok(peak < 80, `expected the burst not to replace the reading, got ${peak}`);
});

test('converges to a sustained new rate within the longest half-life', () => {
  const meter = new TokenRateMeter(words);
  const clock = settle(meter, 6);
  meter.begin(clock);
  stream(meter, clock, clock + 80_000, 20);
  const after = meter.rate(clock + 80_000) ?? 0;
  assert.ok(after > 150, `expected convergence above 150, got ${after}`);
  assert.ok(after < 200, `expected convergence below 200, got ${after}`);
});

test('charges hidden pre-delta reasoning against its silent span', () => {
  const meter = new TokenRateMeter(words);
  // 20s of silence, then a 2-token tool call; the provider bills 1500.
  meter.begin(0);
  meter.push('x y', 20_000);
  meter.end(1500, 20_100);
  closeTo(meter.rate(20_100), 1500 / 20.1, 0);
});

test('never amplifies a visible burst by overhead learned from small tool calls', () => {
  const meter = new TokenRateMeter(words);
  // Ten 25-token tool calls over 2s each, billed 80 (a ~55-token envelope) → ~40 tok/s.
  let clock = 0;
  for (let i = 0; i < 10; i++) {
    meter.begin(clock);
    meter.push('w '.repeat(25), clock + 1500);
    clock += 2000;
    meter.end(80, clock);
    clock += 3000;
  }
  closeTo(meter.rate(clock), 40, -1);
  // A write streams 2000 visible tokens over 20s at a true 100 tok/s. A
  // multiplicative fit (80/25 ≈ 3×) would read ~300; additive stays honest.
  meter.begin(clock);
  let peak = 0;
  for (let t = 0; t < 20_000; t += 100) {
    meter.push('w '.repeat(10), clock + t);
    peak = Math.max(peak, meter.rate(clock + t) ?? 0);
  }
  assert.ok(peak < 115, `expected additive reading below 115, got ${peak}`);
  meter.end(2055, clock + 20_000);
  const settled = meter.rate(clock + 20_000) ?? 0;
  assert.ok(settled > 80, `expected > 80, got ${settled}`);
  assert.ok(settled < 105, `expected < 105, got ${settled}`);
});

test('blanks on reset', () => {
  const meter = new TokenRateMeter(words);
  const clock = settle(meter, 2);
  meter.reset();
  assert.equal(meter.rate(clock), null);
});

test('seed shows a completed turn\'s rate immediately and holds like a settled turn', () => {
  const meter = new TokenRateMeter(words);
  // 600 tokens over 10s (60 tok/s).
  meter.seed(600, 10_000);
  closeTo(meter.rate(1_000), 60, 0);
  closeTo(meter.rate(61_000), 60, 0);
});

test('seed scales small turns past the evidence gate without changing the rate', () => {
  const meter = new TokenRateMeter(words);
  // 120 tokens over 2s (60 tok/s) is below the gate unscaled.
  meter.seed(120, 2_000);
  closeTo(meter.rate(0), 60, 0);
  // Invalid seeds blank the meter instead of throwing or showing garbage.
  meter.seed(0, 2_000);
  assert.equal(meter.rate(0), null);
});

test('a new turn blends with the seeded baseline instead of replacing it', () => {
  const meter = new TokenRateMeter(words);
  meter.seed(600, 10_000);
  meter.begin(5_000);
  // 10s at 30 tok/s against a 60 tok/s baseline: the reading sits between.
  stream(meter, 5_000, 15_000, 3);
  const blended = meter.rate(15_000) ?? 0;
  assert.ok(blended > 30, `expected blend above 30, got ${blended}`);
  assert.ok(blended < 60, `expected blend below 60, got ${blended}`);
});

// ── estimateTokens (D4) ─────────────────────────────────────────────────────

test('estimateTokens counts CJK at 1.2 tokens per character', () => {
  assert.equal(estimateTokens('你好世界'), 4 * 1.2);
});

test('estimateTokens counts other non-whitespace at a quarter token per character', () => {
  assert.equal(estimateTokens('abcd'), 1);
});

test('estimateTokens counts whitespace proportionally, never a spurious zero on blank input', () => {
  // `a b c d`: 4 non-whitespace chars (1 token) + 3 spaces (0.75 tokens).
  assert.equal(estimateTokens('a b c d'), 1.75);
  assert.equal(estimateTokens(''), 0);
  assert.equal(estimateTokens('   '), 0.75);
});

test('estimateTokens mixes scripts additively', () => {
  // 4 CJK (4.8) + "abcd" (1) + one space (0.25) = 6.05.
  assert.ok(Math.abs(estimateTokens('你好世界 abcd') - 6.05) < 1e-9);
});

test('estimateTokens covers every CJK block it claims, and stops at the edge', () => {
  // One char per range: U+3040, U+3400, U+FF00, U+4E00.
  assert.equal(estimateTokens('\u3040\u3400\uFF00\u4E00'), 4 * 1.2);
  // One below each range is outside the estimate: U+303F (Kanbun), U+33FF
  // (CJK Compatibility), U+33FF… and U+4E00's predecessor U+4DFF count as
  // ordinary characters, a quarter token each.
  assert.equal(estimateTokens('\u303F\u33FF\u4DFF'), 3 * 0.25);
});
