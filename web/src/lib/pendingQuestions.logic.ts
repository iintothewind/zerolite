import type { PendingQuestion, WsMessage } from '@/types/api';

/** Grace past the server deadline before a card is dropped locally. */
const EXPIRY_GRACE_MS = 500;

/**
 * The pending question a `question_request` frame describes, or null when the
 * frame is malformed or unanswerable (no choices and no free text).
 */
export function questionFromFrame(
  msg: WsMessage,
  now: number,
  socketVersion: number,
): PendingQuestion | null {
  if (!msg.request_id || !msg.question) return null;
  const choices = Array.isArray(msg.choices)
    ? msg.choices.filter((choice): choice is string => typeof choice === 'string')
    : [];
  const freeText = msg.free_text !== false;
  if (choices.length === 0 && !freeText) return null;
  return {
    requestId: msg.request_id,
    question: msg.question,
    choices,
    multiSelect: msg.multi_select === true,
    freeText,
    timeoutSecs: typeof msg.timeout_secs === 'number' ? msg.timeout_secs : null,
    receivedAt: now,
    socketVersion,
  };
}

export function enqueueQuestion(
  queue: PendingQuestion[],
  question: PendingQuestion,
): PendingQuestion[] {
  if (queue.some((q) => q.requestId === question.requestId)) return queue;
  return [...queue, question];
}

export function dropQuestion(queue: PendingQuestion[], requestId: string): PendingQuestion[] {
  return queue.filter((q) => q.requestId !== requestId);
}

/** The socket that delivered these questions closed; nobody can answer them. */
export function dropSocketQuestions(
  queue: PendingQuestion[],
  socketVersion: number,
): PendingQuestion[] {
  return queue.filter((q) => q.socketVersion !== socketVersion);
}

function expiresAt(q: PendingQuestion): number | null {
  return q.timeoutSecs === null ? null : q.receivedAt + q.timeoutSecs * 1000 + EXPIRY_GRACE_MS;
}

export function dropExpiredQuestions(queue: PendingQuestion[], now: number): PendingQuestion[] {
  return queue.filter((q) => {
    const at = expiresAt(q);
    return at === null || at > now;
  });
}

/** Milliseconds until the earliest question expires, or null when none does. */
export function nextQuestionExpiry(queue: PendingQuestion[], now: number): number | null {
  let earliest: number | null = null;
  for (const q of queue) {
    const at = expiresAt(q);
    if (at !== null && (earliest === null || at < earliest)) earliest = at;
  }
  return earliest === null ? null : Math.max(0, earliest - now);
}
