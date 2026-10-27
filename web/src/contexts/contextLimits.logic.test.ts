import assert from 'node:assert/strict';
import test from 'node:test';

import { resolveContextBarState } from '../lib/contextBar.ts';
import { contextLimitsFromFrame } from './contextLimits.logic.ts';

test('a done frame without capacity clears the previous route denominator', () => {
  let limits = contextLimitsFromFrame({
    max_context_tokens: 180_000,
    model_context_window: 200_000,
    effective_context_window: 200_000,
  });
  assert.deepEqual(limits, {
    effectiveWindow: 200_000,
    trimBudgetTokens: 180_000,
    modelWindow: 200_000,
  });

  limits = contextLimitsFromFrame({
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
  const limits = contextLimitsFromFrame({
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
  const limits = contextLimitsFromFrame({
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

// A mid-turn model switch: the new route's window and budget must be readable
// from the very frame that reports its measurement, because a turn can switch
// models and keep running. The numbers are the reported field case: a 1M window
// at the default 80% budget.
test('a usage frame carries the serving route window and budget', () => {
  assert.deepEqual(
    contextLimitsFromFrame({
      max_context_tokens: 838_860,
      model_context_window: 1_048_576,
      effective_context_window: 1_048_576,
    }),
    { effectiveWindow: 1_048_576, trimBudgetTokens: 838_860, modelWindow: 1_048_576 },
  );
});

// An older daemon sends none of the three fields. That is "no route
// information here", not "the route has no window": the caller must keep the
// snapshot it already has instead of blanking the ring.
test('a frame without route fields yields an empty snapshot', () => {
  assert.deepEqual(contextLimitsFromFrame({}), {
    effectiveWindow: null,
    trimBudgetTokens: null,
    modelWindow: null,
  });
});
