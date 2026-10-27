import assert from 'node:assert/strict';
import test from 'node:test';

import type { WsMessage } from '../types/api';
import { contextMeterStep } from './contextMeter.logic.ts';

// The symptom these pin: a trim lands before the request goes out, and the ring
// must show the trimmed size right then — not the number that triggered it, and
// not wait for the provider's reply.

test('a mid-turn trim moves the ring to the post-trim size, labelled estimate', () => {
  const trim = {
    type: 'history_trimmed',
    tokens_after: 92_987,
    tokens_before: 105_706,
  } as unknown as WsMessage;
  assert.deepEqual(contextMeterStep(trim), { used: 92_987, source: 'estimate' });
});

test('a trim without token accounting does not touch the ring', () => {
  assert.equal(contextMeterStep({ type: 'history_trimmed' } as unknown as WsMessage), null);
});

// The turn-start frame: the ring moves on the assembled request, before any
// provider reply exists.
test('context_usage carries the assembled size and its wire label', () => {
  const calibrated = {
    type: 'context_usage',
    input_tokens: 92_987,
    input_tokens_source: 'calibrated',
  } as unknown as WsMessage;
  // No route fields on this frame: the caller keeps the snapshot it has.
  assert.deepEqual(contextMeterStep(calibrated), { used: 92_987, source: 'calibrated' });

  // An unlabelled daemon publishes an estimate; the meter never claims
  // `provider` for a number no provider sent.
  const unlabelled = { type: 'context_usage', input_tokens: 512 } as unknown as WsMessage;
  assert.deepEqual(contextMeterStep(unlabelled)?.used, 512);
  assert.equal(contextMeterStep(unlabelled)?.source, 'estimate');
});

test('a provider measurement replaces the estimate and claims provider', () => {
  const usage = { type: 'usage', input_tokens: 105_706 } as unknown as WsMessage;
  assert.deepEqual(contextMeterStep(usage), { used: 105_706, source: 'provider' });
});

test('done publishes the size the turn ended with, keeping the label', () => {
  const done = { type: 'done', last_input_tokens: 92_987 } as unknown as WsMessage;
  assert.deepEqual(contextMeterStep(done), { used: 92_987 });
});

test('done with no provider usage clears the numerator instead of resurrecting an older figure', () => {
  const cleared = { type: 'done', last_input_tokens: null } as unknown as WsMessage;
  assert.deepEqual(contextMeterStep(cleared), { used: null });

  // Older daemons send no `last_input_tokens` at all: fall back to the frame's
  // accumulated count rather than clearing.
  const legacy = { type: 'done', input_tokens: 4_000 } as unknown as WsMessage;
  assert.equal(contextMeterStep(legacy)?.used, 4_000);
});

test('frames that do not price the request leave the ring alone', () => {
  for (const type of ['chunk', 'thinking', 'tool_call', 'tool_result', 'agent_start']) {
    assert.equal(contextMeterStep({ type } as unknown as WsMessage), null, type);
  }
});

// The reported bug: a model switch that happens mid-turn (a tool asked for
// another route) kept the previous route's `limit` / `trim budget` on screen
// until the turn ended, while the runtime had been trimming against the new
// route from its first post-switch dispatch. The route snapshot must travel on
// the frame that reports the new route's measurement.
test('a mid-turn model switch reaches the ring on the usage frame, not on done', () => {
  const smallRoute = {
    type: 'usage',
    input_tokens: 14_900,
  } as unknown as WsMessage;
  // Nothing to adopt from a frame without the snapshot.
  assert.equal(contextMeterStep(smallRoute)?.limits, undefined);

  const bigRoute = {
    type: 'usage',
    input_tokens: 14_900,
    max_context_tokens: 838_860,
    model_context_window: 1_048_576,
    effective_context_window: 1_048_576,
  } as unknown as WsMessage;
  assert.deepEqual(contextMeterStep(bigRoute), {
    used: 14_900,
    source: 'provider',
    limits: {
      effectiveWindow: 1_048_576,
      trimBudgetTokens: 838_860,
      modelWindow: 1_048_576,
    },
  });
});

// An older daemon sends none of the three route fields. That is "no route
// information on this frame", not "the route has no window": the caller keeps
// the snapshot it already has instead of blanking the ring.
test('a priced frame without route fields reports no limits to adopt', () => {
  const trim = { type: 'history_trimmed', tokens_after: 1 } as unknown as WsMessage;
  assert.equal(contextMeterStep(trim)?.limits, undefined);
});
