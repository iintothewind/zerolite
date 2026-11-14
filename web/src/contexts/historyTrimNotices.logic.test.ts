import assert from 'node:assert/strict';
import test from 'node:test';

import {
  buildHistoryTrimmedNotice,
  type HistoryTrimmedNoticeMessage,
} from './historyTrimNotices.logic.ts';

// English strings mirrored from `@/lib/i18n` so the branch logic is tested
// without a browser/React harness.
const en: Record<string, string> = {
  'agent.history_trimmed': 'History trimmed: dropped {dropped} messages, kept {kept} turns',
  'agent.history_trimmed_turns': 'History trimmed: dropped {dropped} older {dropped_unit}, kept {kept}',
  'agent.history_trimmed_turn_singular': 'turn',
  'agent.history_trimmed_turn_plural': 'turns',
  'agent.history_trimmed_size': '{notice} · {before} → {after} tokens',
  'agent.history_trimmed_size_budget': '{notice} · {before} → {after} tokens (budget {budget})',
  'agent.history_trimmed_floor':
    'History cannot be trimmed further: the newest turn needs {after} tokens, over the {budget}-token context window',
};
const t = (key: string) => en[key] ?? key;

test('combined turn and token accounting preserves units, sources and floor precedence', () => {
  const event: HistoryTrimmedNoticeMessage = {
    dropped_messages: 12, dropped_turns: 1, kept_turns: 2,
    reason: 'context token budget exceeded', token_budget: 10000,
    tokens_before: 20000, tokens_after: 6000,
    tokens_before_source: 'provider', tokens_after_source: 'calibrated',
  };
  const text = buildHistoryTrimmedNotice(event, t);
  assert.equal(text, 'History trimmed: dropped 1 older turn, kept 2 · 20K → 6K tokens (budget 10K)');
  const zero = buildHistoryTrimmedNotice({ ...event, dropped_turns: 0 }, t);
  assert.match(zero, /dropped 0 older turns/);
  const floor = buildHistoryTrimmedNotice({ ...event, tokens_after: 12000, unsatisfiable_floor: true }, t);
  assert.equal(
    floor,
    'History cannot be trimmed further: the newest turn needs 12K tokens, over the 10K-token context window',
  );
});

test('a trim without a budget omits the budget clause', () => {
  const text = buildHistoryTrimmedNotice(
    {
      reason: 'context window overflow recovery',
      dropped_messages: 4,
      kept_turns: 2,
      token_budget: undefined,
      tokens_before: 612000,
      tokens_after: 117000,
    } satisfies HistoryTrimmedNoticeMessage,
    t,
  );
  assert.equal(text, 'History trimmed: dropped 4 messages, kept 2 turns · 612K → 117K tokens');
});

test('message-limit trim without token accounting keeps the count-only fallback', () => {
  const text = buildHistoryTrimmedNotice(
    {
      reason: 'history message limit exceeded',
      dropped_messages: 12,
      kept_turns: 3,
    } satisfies HistoryTrimmedNoticeMessage,
    t,
  );
  assert.equal(text, 'History trimmed: dropped 12 messages, kept 3 turns');
});

test('untrimmable newest-turn floor renders a distinct notice that does not claim history was trimmed', () => {
  const text = buildHistoryTrimmedNotice(
    {
      reason: 'context token budget exceeded',
      dropped_messages: 0,
      kept_turns: 1,
      token_budget: 100000,
      tokens_before: 612000,
      tokens_after: 117000,
      unsatisfiable_floor: true,
    } satisfies HistoryTrimmedNoticeMessage,
    t,
  );
  assert.equal(
    text,
    'History cannot be trimmed further: the newest turn needs 117K tokens, over the 100K-token context window',
  );
});

test('floor reached after real drops reports the unsatisfiable outcome, not a successful trim', () => {
  const text = buildHistoryTrimmedNotice(
    {
      reason: 'context token budget exceeded',
      dropped_messages: 2,
      kept_turns: 1,
      token_budget: 100000,
      tokens_before: 612000,
      tokens_after: 117000,
      unsatisfiable_floor: true,
    } satisfies HistoryTrimmedNoticeMessage,
    t,
  );
  assert.equal(
    text,
    'History cannot be trimmed further: the newest turn needs 117K tokens, over the 100K-token context window',
  );
});

test('ordinary trim with dropped messages keeps the trimmed wording even when slightly over budget', () => {
  const text = buildHistoryTrimmedNotice(
    {
      reason: 'context token budget exceeded',
      dropped_messages: 1,
      kept_turns: 1,
      token_budget: 100000,
      tokens_before: 612000,
      tokens_after: 117000,
    } satisfies HistoryTrimmedNoticeMessage,
    t,
  );
  assert.match(text, /^History trimmed: /);
  assert.ok(!text.includes('cannot be trimmed'));
});

test('an estimated count is marked, a measured one is not', () => {
  const text = buildHistoryTrimmedNotice(
    {
      dropped_messages: 2,
      dropped_turns: 1,
      kept_turns: 1,
      token_budget: 108000,
      tokens_before: 112146,
      tokens_after: 24155,
      tokens_before_source: 'calibrated',
      tokens_after_source: 'estimate',
    } satisfies HistoryTrimmedNoticeMessage,
    t,
  );
  assert.equal(text, 'History trimmed: dropped 1 older turn, kept 1 · 112.1K → ~24.2K tokens (budget 108K)');
});
