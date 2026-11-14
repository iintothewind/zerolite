import assert from 'node:assert/strict';
import test from 'node:test';

import { formatHistoryTrimmedNotice } from './historyTrimNotice.ts';

const strings: Record<string, string> = {
  'agent.history_trimmed': 'Trimmed: dropped {dropped} messages, kept {kept} turns',
  'agent.history_trimmed_turns': 'Trimmed: dropped {dropped} older {dropped_unit}, kept {kept}',
  'agent.history_trimmed_turn_singular': 'turn',
  'agent.history_trimmed_turn_plural': 'turns',
};
const translate = (key: string) => strings[key] ?? key;

test('formats whole-turn accounting with singular grammar', () => {
  const notice = formatHistoryTrimmedNotice({
    type: 'history_trimmed',
    dropped_messages: 2,
    dropped_turns: 1,
    kept_turns: 1,
    reason: 'turn limit',
  }, translate);

  assert.equal(notice, 'Trimmed: dropped 1 older turn, kept 1');
});

test('falls back to legacy message accounting when dropped_turns is absent', () => {
  const notice = formatHistoryTrimmedNotice({
    type: 'history_trimmed',
    dropped_messages: 4,
    kept_turns: 2,
    reason: 'message limit',
  }, translate);

  assert.equal(notice, 'Trimmed: dropped 4 messages, kept 2 turns');
});
