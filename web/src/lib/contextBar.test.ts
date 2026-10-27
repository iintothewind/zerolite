import assert from 'node:assert/strict';
import test from 'node:test';

import { resolveContextBarState } from './contextBar.ts';

test('uses the effective window as denominator and marks the trim budget', () => {
  // (effectiveWindow, trimBudgetTokens, used)
  const state = resolveContextBarState(200_000, 180_000, 100_000);
  assert.ok(state);
  assert.equal(state.denominator, 200_000);
  assert.equal(state.percent, 50);
  assert.equal(state.trimMarkerIndex, 14);
  assert.equal(state.cells[14], '│');
});

test('a trim budget at the effective window has no marker', () => {
  const state = resolveContextBarState(32_000, 32_000, 16_000);
  assert.ok(state);
  assert.equal(state.denominator, 32_000);
  assert.equal(state.percent, 50);
  assert.equal(state.trimMarkerIndex, null);
});

test('no effective window fabricates no denominator', () => {
  assert.equal(resolveContextBarState(0, null, 100), null);
  assert.equal(resolveContextBarState(null, null, 100), null);
});
