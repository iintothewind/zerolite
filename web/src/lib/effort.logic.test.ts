import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  EMPTY_REASONING,
  INITIAL_EFFORT_STATE,
  chooseEffort,
  parseReasoningInfo,
  publishReasoning,
} from './effort.logic.ts';

const model = (key: string, levels: string[], rejected = false) =>
  parseReasoningInfo({ key, levels, rejected });

test('a model starts at auto and keeps the level chosen for it', () => {
  let state = publishReasoning(INITIAL_EFFORT_STATE, model('a.b:m', ['auto', 'low', 'high']));
  assert.equal(state.effort, 'auto');
  state = chooseEffort(state, 'high');
  // The next `done` frame republishes the same levels.
  state = publishReasoning(state, model('a.b:m', ['auto', 'low', 'high']));
  assert.equal(state.effort, 'high');
});

test('switching models resets the choice to auto', () => {
  let state = publishReasoning(INITIAL_EFFORT_STATE, model('a.b:m', ['auto', 'high']));
  state = chooseEffort(state, 'high');
  state = publishReasoning(state, model('a.c:n', ['auto', 'high']));
  assert.equal(state.effort, 'auto');
});

test('a level the endpoint refused falls back to auto and stays on offer', () => {
  let state = publishReasoning(INITIAL_EFFORT_STATE, model('a.b:m', ['auto', 'high']));
  state = chooseEffort(state, 'high');
  state = publishReasoning(state, model('a.b:m', ['auto', 'high'], true));
  assert.equal(state.effort, 'auto');
  assert.equal(chooseEffort(state, 'high').effort, 'high');
});

test('a level the model does not offer cannot be chosen', () => {
  const state = publishReasoning(INITIAL_EFFORT_STATE, model('a.b:m', ['auto', 'low']));
  assert.equal(chooseEffort(state, 'max').effort, 'auto');
});

test('wire levels are filtered to known ids in ladder order', () => {
  assert.deepEqual(model('k', ['high', 'turbo', 'off', 'auto', 'high']).levels, ['auto', 'off', 'high']);
  assert.equal(parseReasoningInfo(undefined), EMPTY_REASONING);
  assert.equal(parseReasoningInfo({ key: 'k' }), EMPTY_REASONING);
  assert.equal(model('k', ['turbo']), EMPTY_REASONING);
});
