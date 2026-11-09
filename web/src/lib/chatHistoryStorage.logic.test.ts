import assert from 'node:assert/strict';
import test from 'node:test';

import {
  mapServerMessagesToPersisted,
  parseNativeAssistantToolPayload,
  parseNativeToolResultEnvelope,
  uiMessagesToPersisted,
} from './chatHistoryStorage.logic.ts';

test('parseNativeAssistantToolPayload extracts text and tool_calls', () => {
  const payload = JSON.stringify({
    content: 'Let me read that file.',
    tool_calls: [
      { id: 'call_1', name: 'file_read', arguments: '{"path":"a.ts"}' },
    ],
  });
  const parsed = parseNativeAssistantToolPayload(payload);
  assert.ok(parsed);
  assert.equal(parsed!.text, 'Let me read that file.');
  assert.equal(parsed!.toolCalls.length, 1);
  assert.equal(parsed!.toolCalls[0]!.name, 'file_read');
  assert.equal(parsed!.toolCalls[0]!.id, 'call_1');
  assert.deepEqual(parsed!.toolCalls[0]!.args, { path: 'a.ts' });
});

test('parseNativeAssistantToolPayload returns null for plain assistant text', () => {
  assert.equal(parseNativeAssistantToolPayload('hello world'), null);
  assert.equal(parseNativeAssistantToolPayload('{"content":"only text"}'), null);
});

test('parseNativeToolResultEnvelope unwraps content + tool_call_id', () => {
  const envelope = JSON.stringify({
    content: 'file contents here',
    tool_call_id: 'call_1',
  });
  assert.deepEqual(parseNativeToolResultEnvelope(envelope), {
    toolCallId: 'call_1',
    output: 'file contents here',
  });
});

test('mapServerMessagesToPersisted rebuilds ToolCallCards instead of raw JSON', () => {
  const before = Date.now();
  const assistant = JSON.stringify({
    content: null,
    tool_calls: [
      { id: 'chatcmpl-tool-af8408577ab431c3', name: 'file_read', arguments: '{"path":"build.sh"}' },
      { id: 'call_2', name: 'shell', arguments: '{"command":"ls"}' },
      { id: 'call_3', name: 'shell', arguments: '{"command":"pwd"}' },
      { id: 'call_4', name: 'shell', arguments: '{"command":"echo hi"}' },
      { id: 'call_5', name: 'shell', arguments: '{"command":"date"}' },
    ],
  });
  const tool = JSON.stringify({
    content: '1758:         IFS=$_oldifs',
    tool_call_id: 'chatcmpl-tool-af8408577ab431c3',
  });

  // 3 rows → expands to user + 5 cards + Done = 7 bubbles (more than rows).
  const bubbles = mapServerMessagesToPersisted([
    { role: 'user', content: 'read the build script', created_at: null },
    { role: 'assistant', content: assistant, created_at: null },
    { role: 'tool', content: tool, created_at: null },
    { role: 'assistant', content: 'Done reading.', created_at: null },
  ]);
  const after = Date.now();

  assert.equal(bubbles.length, 7);
  assert.equal(bubbles[0]!.role, 'user');
  assert.equal(bubbles[1]!.toolCall?.name, 'file_read');
  assert.equal(bubbles[1]!.toolCall?.id, 'chatcmpl-tool-af8408577ab431c3');
  assert.equal(bubbles[1]!.toolCall?.output, '1758:         IFS=$_oldifs');
  assert.equal(bubbles[6]!.content, 'Done reading.');
  assert.equal(bubbles[6]!.markdown, true);

  // No raw JSON blobs left in agent content.
  for (const b of bubbles) {
    if (b.toolCall) {
      assert.equal(b.content, '');
    } else {
      assert.ok(!b.content.includes('tool_call_id'));
    }
  }

  // Timestamps use final bubble count — last ≈ now-1s, never in the future.
  const last = bubbles[bubbles.length - 1]!;
  const lastMs = Date.parse(last.timestamp);
  assert.ok(lastMs <= after);
  assert.ok(lastMs >= before - 2000);
  for (let i = 1; i < bubbles.length; i++) {
    const prev = Date.parse(bubbles[i - 1]!.timestamp);
    const cur = Date.parse(bubbles[i]!.timestamp);
    assert.equal(cur - prev, 1000);
  }
});

test('mapServerMessagesToPersisted orphan tool result still becomes a card', () => {
  const tool = JSON.stringify({
    content: 'orphan output',
    tool_call_id: 'missing',
  });
  const bubbles = mapServerMessagesToPersisted([
    { role: 'tool', content: tool, created_at: null },
  ]);
  assert.equal(bubbles.length, 1);
  assert.equal(bubbles[0]!.toolCall?.output, 'orphan output');
  assert.equal(bubbles[0]!.toolCall?.id, 'missing');
  assert.ok(!bubbles[0]!.content.includes('tool_call_id'));
});

// ── Machinery rows must not render as operator input ────────────────────────
//
// Prompt-mode tool rounds and the trim breadcrumb are persisted as `user` rows
// because that is how they are fed to the model. Mapping every `user` row to a
// user bubble showed each tool round as if the operator had typed it, and the
// trim breadcrumb as an operator message. Which rows those are is the runtime's
// call, reported as `synthetic` — asserted in `history_trim.rs`.

test('a prompt-mode tool round is not an operator bubble', () => {
  const bubbles = mapServerMessagesToPersisted([
    { role: 'user', content: 'read the build script', created_at: null },
    {
      role: 'user',
      content: '[Tool results]\n<tool_result>ok</tool_result>',
      synthetic: true,
      created_at: null,
    },
    { role: 'assistant', content: 'Done reading.', created_at: null },
  ]);
  assert.equal(bubbles.length, 2, 'the tool round is dropped, the real turn survives');
  assert.deepEqual(bubbles.map((b) => b.content), ['read the build script', 'Done reading.']);
});

test('the trim breadcrumb is not an operator bubble', () => {
  const bubbles = mapServerMessagesToPersisted([
    {
      role: 'user',
      content: '[earlier turns omitted to fit the context window]',
      synthetic: true,
      created_at: null,
    },
    { role: 'user', content: 'what did we decide?', created_at: null },
  ]);
  assert.deepEqual(bubbles.map((b) => b.content), ['what did we decide?']);
});

test('an operator message that merely looks like machinery still renders', () => {
  // The decision is the gateway's flag, not a string match here: text that
  // reads like a tool round is still the operator's if the runtime did not
  // write it.
  const bubbles = mapServerMessagesToPersisted([
    { role: 'user', content: '[Tool results] is what the log says', created_at: null },
  ]);
  assert.deepEqual(bubbles.map((b) => b.content), ['[Tool results] is what the log says']);
});

test('a tool round between two real turns keeps both turns', () => {
  const bubbles = mapServerMessagesToPersisted([
    { role: 'user', content: 'first', created_at: null },
    { role: 'assistant', content: 'calling', created_at: null },
    {
      role: 'user',
      content: '[Tool results]\n<tool_result>r</tool_result>',
      synthetic: true,
      created_at: null,
    },
    { role: 'assistant', content: 'done', created_at: null },
    { role: 'user', content: 'second', created_at: null },
    { role: 'assistant', content: 'done again', created_at: null },
  ]);
  assert.deepEqual(
    bubbles.map((b) => `${b.role}:${b.content}`),
    ['user:first', 'agent:calling', 'agent:done', 'user:second', 'agent:done again'],
  );
});

test('uiMessagesToPersisted drops trajectory carriers but keeps reasoning-only bubbles', () => {
  const timestamp = new Date(0);
  const messages = [
    {
      id: 'carrier',
      role: 'agent' as const,
      content: '',
      timestamp,
      segments: { steps: [{ thinking: '', text: '', toolCalls: [{ name: 'shell', id: 'c1' }] }] },
    },
    { id: 'reasoning', role: 'agent' as const, content: '', thinking: 'why', timestamp },
    { id: 'card', role: 'agent' as const, content: '', toolCall: { name: 'shell', id: 'c1' }, timestamp },
  ];
  const persisted = uiMessagesToPersisted(messages);
  assert.deepEqual(
    persisted.map((m) => m.id),
    ['reasoning', 'card'],
  );
  assert.ok(persisted.every((m) => m.content !== '' || m.thinking || m.toolCall));
});
