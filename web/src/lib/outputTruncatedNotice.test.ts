import assert from 'node:assert/strict';
import test from 'node:test';

import { formatOutputTruncatedNotice } from './outputTruncatedNotice.ts';

const strings: Record<string, string> = {
  'agent.output_truncated': 'Cut: {reason}, {tokens} tokens from {model}.',
  'agent.output_truncated_no_tokens': 'Cut: {reason}, from {model}.',
};
const translate = (key: string) => strings[key] ?? key;

test('quotes the provider-reported output tokens', () => {
  const notice = formatOutputTruncatedNotice({
    type: 'output_truncated',
    reason: 'length',
    model: 'nv-muse-glimmer-30b',
    output_tokens: 890,
  }, translate);

  assert.equal(notice, 'Cut: length, 890 tokens from nv-muse-glimmer-30b.');
});

test('omits the token count when the provider reported none', () => {
  const notice = formatOutputTruncatedNotice({
    type: 'output_truncated',
    reason: 'length',
    model: 'm',
  }, translate);

  assert.equal(notice, 'Cut: length, from m.');
});
