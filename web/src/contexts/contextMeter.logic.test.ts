import assert from 'node:assert/strict';
import test from 'node:test';

import type { WsMessage } from '../types/api';
import { meterUpdateFromFrame } from './contextMeter.logic.ts';

// Frames are narrowed by `type` inside the reducer; the fixtures below carry
// only the fields each case is about.

// The symptom this pins: a trim lands before the request goes out, and the ring
// must show the trimmed size right then — not the number that triggered it, and
// not wait for the provider's reply.
test('a mid-turn trim moves the ring to the post-trim size, labelled estimate', () => {
  const trim = {
    type: 'history_trimmed',
    tokens_after: 92_987,
    tokens_before: 105_706,
  } as unknown as WsMessage;
  assert.deepEqual(meterUpdateFromFrame(trim), { used: 92_987, source: 'estimate' });
});

test('a trim without token accounting does not touch the ring', () => {
  assert.equal(meterUpdateFromFrame({ type: 'history_trimmed' } as unknown as WsMessage), null);
});

// The turn-start frame: the ring moves on the assembled request, before any
// provider reply exists.
test('context_usage carries the assembled size and its wire label', () => {
  const calibrated = {
    type: 'context_usage',
    input_tokens: 92_987,
    input_tokens_source: 'calibrated',
  } as unknown as WsMessage;
  assert.deepEqual(meterUpdateFromFrame(calibrated), { used: 92_987, source: 'calibrated' });

  // An unlabelled daemon publishes an estimate; the meter never claims
  // `provider` for a number no provider sent.
  const unlabelled = { type: 'context_usage', input_tokens: 512 } as unknown as WsMessage;
  assert.deepEqual(meterUpdateFromFrame(unlabelled), { used: 512, source: 'estimate' });
});

test('a provider measurement replaces the estimate and claims provider', () => {
  const usage = { type: 'usage', input_tokens: 105_706 } as unknown as WsMessage;
  assert.deepEqual(meterUpdateFromFrame(usage), { used: 105_706, source: 'provider' });
});

test('done publishes the size the turn ended with, keeping the label', () => {
  const done = { type: 'done', last_input_tokens: 92_987 } as unknown as WsMessage;
  const update = meterUpdateFromFrame(done);
  assert.equal(update?.used, 92_987);
  assert.equal(update?.source, undefined, 'done keeps the label it inherited');
});

test('done with no provider usage clears the numerator instead of resurrecting an older figure', () => {
  const cleared = { type: 'done', last_input_tokens: null } as unknown as WsMessage;
  assert.deepEqual(meterUpdateFromFrame(cleared), { used: null });

  // Older daemons send no `last_input_tokens` at all: fall back to the frame's
  // accumulated count rather than clearing.
  const legacy = { type: 'done', input_tokens: 4_000 } as unknown as WsMessage;
  assert.equal(meterUpdateFromFrame(legacy)?.used, 4_000);
});

test('frames that do not price the request leave the ring alone', () => {
  for (const type of ['chunk', 'thinking', 'tool_call', 'tool_result', 'agent_start']) {
    assert.equal(meterUpdateFromFrame({ type } as unknown as WsMessage), null, type);
  }
});
