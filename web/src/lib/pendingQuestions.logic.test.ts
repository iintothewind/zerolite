import assert from 'node:assert/strict';
import test from 'node:test';

import type { PendingQuestion } from '../types/api.ts';
import {
  dropExpiredQuestions,
  dropQuestion,
  dropSocketQuestions,
  enqueueQuestion,
  nextQuestionExpiry,
  questionFromFrame,
} from './pendingQuestions.logic.ts';

function question(overrides: Partial<PendingQuestion> = {}): PendingQuestion {
  return {
    requestId: 'q-1',
    question: 'Which?',
    choices: ['A', 'B'],
    multiSelect: false,
    freeText: true,
    timeoutSecs: 300,
    receivedAt: 1_000,
    socketVersion: 1,
    ...overrides,
  };
}

test('a frame without choices or flags defaults to a free-form question', () => {
  const q = questionFromFrame(
    { type: 'question_request', request_id: 'q-1', question: 'Name?' },
    5,
    2,
  );
  assert.deepEqual(q, {
    requestId: 'q-1',
    question: 'Name?',
    choices: [],
    multiSelect: false,
    freeText: true,
    timeoutSecs: null,
    receivedAt: 5,
    socketVersion: 2,
  });
});

test('a frame keeps its string choices, flags and timeout', () => {
  const q = questionFromFrame(
    {
      type: 'question_request',
      request_id: 'q-1',
      question: 'Which?',
      choices: ['A', 7 as unknown as string, 'B'],
      multi_select: true,
      free_text: false,
      timeout_secs: 60,
    },
    0,
    1,
  );
  assert.deepEqual(q?.choices, ['A', 'B']);
  assert.equal(q?.multiSelect, true);
  assert.equal(q?.freeText, false);
  assert.equal(q?.timeoutSecs, 60);
});

test('a frame without an id or a question, or with nothing to answer, is rejected', () => {
  assert.equal(questionFromFrame({ type: 'question_request', question: 'Q?' }, 0, 1), null);
  assert.equal(questionFromFrame({ type: 'question_request', request_id: 'q' }, 0, 1), null);
  assert.equal(
    questionFromFrame(
      { type: 'question_request', request_id: 'q', question: 'Q?', free_text: false },
      0,
      1,
    ),
    null,
  );
});

test('a duplicate request id leaves the queue as it was', () => {
  const queue = enqueueQuestion([], question());
  assert.equal(enqueueQuestion(queue, question({ question: 'again' })), queue);
  assert.deepEqual(
    enqueueQuestion(queue, question({ requestId: 'q-2' })).map((q) => q.requestId),
    ['q-1', 'q-2'],
  );
});

test('an answered question leaves the queue', () => {
  const queue = [question(), question({ requestId: 'q-2' })];
  assert.deepEqual(dropQuestion(queue, 'q-1').map((q) => q.requestId), ['q-2']);
});

test('a closed socket drops only its own questions', () => {
  const queue = [question(), question({ requestId: 'q-2', socketVersion: 2 })];
  assert.deepEqual(dropSocketQuestions(queue, 1).map((q) => q.requestId), ['q-2']);
});

test('a question without a deadline never expires', () => {
  const queue = [question({ timeoutSecs: null })];
  assert.equal(dropExpiredQuestions(queue, Number.MAX_SAFE_INTEGER).length, 1);
  assert.equal(nextQuestionExpiry(queue, 0), null);
});

test('a question expires half a second past its deadline', () => {
  const queue = [question({ timeoutSecs: 10 })];
  assert.equal(dropExpiredQuestions(queue, 1_000 + 10_000 + 499).length, 1);
  assert.equal(dropExpiredQuestions(queue, 1_000 + 10_000 + 500).length, 0);
});

test('the next expiry is the earliest deadline, never negative', () => {
  const queue = [
    question({ requestId: 'late', timeoutSecs: 60 }),
    question({ requestId: 'soon', timeoutSecs: 5 }),
    question({ requestId: 'open', timeoutSecs: null }),
  ];
  assert.equal(nextQuestionExpiry(queue, 1_000), 5_500);
  assert.equal(nextQuestionExpiry(queue, 1_000_000), 0);
});
