import assert from 'node:assert/strict';
import test from 'node:test';

import { resolveContextBarState } from '../lib/contextBar.ts';
import { contextLimitsFromDoneFrame } from './contextLimits.logic.ts';

test('a done frame without capacity clears the previous route denominator', () => {
  let limits = contextLimitsFromDoneFrame({
    max_context_tokens: 180_000,
    model_context_window: 200_000,
    effective_context_window: 200_000,
  });
  assert.deepEqual(limits, {
    effectiveWindow: 200_000,
    trimBudgetTokens: 180_000,
    modelWindow: 200_000,
  });

  limits = contextLimitsFromDoneFrame({
    max_context_tokens: 32_000,
  });
  assert.deepEqual(limits, {
    effectiveWindow: 32_000,
    trimBudgetTokens: 32_000,
    modelWindow: null,
  });

  const rendered = resolveContextBarState(
    limits.effectiveWindow,
    limits.trimBudgetTokens,
    16_000,
  );
  assert.ok(rendered);
  assert.equal(rendered.denominator, 32_000);
  assert.equal(rendered.trimMarkerIndex, null);
});

test('the meter denominator is the effective window, not the trim budget', () => {
  // The gateway publishes the clamped window and the trim budget separately.
  const limits = contextLimitsFromDoneFrame({
    max_context_tokens: 104_857, // trim budget: 80% of 131_072
    model_context_window: 131_072,
    effective_context_window: 131_072,
  });
  assert.equal(limits.effectiveWindow, 131_072);
  assert.equal(limits.trimBudgetTokens, 104_857);

  const rendered = resolveContextBarState(
    limits.effectiveWindow,
    limits.trimBudgetTokens,
    65_536,
  );
  assert.ok(rendered);
  assert.equal(rendered.denominator, 131_072);
  assert.equal(rendered.percent, 50);
  // The budget is a trigger, so it is marked inside the bar, not the cap.
  assert.ok(rendered.trimMarkerIndex !== null);
  assert.ok(rendered.trimMarkerIndex < 16);
});

test('a configured ceiling clamps the effective window the meter shows', () => {
  const limits = contextLimitsFromDoneFrame({
    max_context_tokens: 80_000, // trim budget: 80% of the clamped 100_000
    model_context_window: 131_072,
    effective_context_window: 100_000,
  });
  assert.equal(limits.effectiveWindow, 100_000);
  const rendered = resolveContextBarState(
    limits.effectiveWindow,
    limits.trimBudgetTokens,
    50_000,
  );
  assert.ok(rendered);
  assert.equal(rendered.denominator, 100_000);
});
