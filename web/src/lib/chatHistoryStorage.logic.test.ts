import assert from 'node:assert/strict';
import test from 'node:test';

import type { SessionMessageRow } from '../types/api.ts';
import {
  mapServerMessagesToPersisted,
  uiMessagesToPersisted,
} from './chatHistoryStorage.logic.ts';

let nextId = 1;
function row(fields: Partial<SessionMessageRow> & Pick<SessionMessageRow, 'kind'>): SessionMessageRow {
  const role = fields.kind === 'tool_result' ? 'tool' : fields.kind === 'user' ? 'user' : 'assistant';
  return {
    id: nextId++,
    turn: 1,
    role,
    content: '',
    content_bytes: 0,
    tool_name: null,
    tool_call_id: null,
    created_at: null,
    ...fields,
  };
}

test('mapServerMessagesToPersisted folds kind rows into bubbles and cards', () => {
  const at = '2026-11-10T01:02:03.000Z';
  const rows = [
    row({ kind: 'user', content: 'read the build script', turn: 3, created_at: at }),
    row({ kind: 'step', content: 'Let me look.', turn: 3, created_at: at }),
    row({
      kind: 'tool_call',
      content: '{"path":"build.rs"}',
      tool_name: 'file_read',
      tool_call_id: 'call_1',
      turn: 3,
      created_at: at,
    }),
    row({
      kind: 'tool_result',
      content: null,
      content_bytes: 7,
      tool_name: 'file_read',
      tool_call_id: 'call_1',
      turn: 3,
      created_at: at,
    }),
    row({ kind: 'runtime', content: '[Tool results] ok', turn: 3, created_at: at }),
    row({ kind: 'assistant', content: 'It builds.', turn: 3, created_at: at }),
  ];
  const resultId = rows[3]!.id;
  const bubbles = mapServerMessagesToPersisted(rows);

  assert.deepEqual(
    bubbles.map((b) => (b.toolCall ? `card:${b.toolCall.name}` : `${b.role}:${b.content}`)),
    ['user:read the build script', 'agent:Let me look.', 'card:file_read', 'agent:It builds.'],
  );
  const card = bubbles[2]!.toolCall!;
  assert.deepEqual(card.args, { path: 'build.rs' });
  assert.equal(card.id, 'call_1');
  assert.deepEqual(card.storedOutput, { messageId: resultId, bytes: 7 });
  assert.equal(card.output, undefined);
  assert.ok(bubbles.every((b) => b.turn === 3));
  assert.ok(bubbles.every((b) => b.timestamp === at));
});

test('an orphan tool_result still becomes a card', () => {
  const bubbles = mapServerMessagesToPersisted([
    row({
      kind: 'tool_result',
      content: null,
      content_bytes: 5,
      tool_name: 'shell',
      tool_call_id: 'missing',
    }),
  ]);
  assert.equal(bubbles.length, 1);
  assert.equal(bubbles[0]!.toolCall!.name, 'shell');
  assert.equal(bubbles[0]!.toolCall!.id, 'missing');
  assert.equal(bubbles[0]!.toolCall!.storedOutput!.bytes, 5);
});

test('a tool_result with content fills the output directly', () => {
  const bubbles = mapServerMessagesToPersisted([
    row({ kind: 'tool_call', content: '{}', tool_name: 'shell', tool_call_id: 'c1' }),
    row({
      kind: 'tool_result',
      content: 'one\n',
      content_bytes: 4,
      tool_name: 'shell',
      tool_call_id: 'c1',
    }),
  ]);
  assert.equal(bubbles.length, 1);
  assert.equal(bubbles[0]!.toolCall!.output, 'one\n');
});

test('a result never lands on a card whose output is already stored', () => {
  // Two calls; the second result carries an id no card has. It must not fall
  // back onto the first card, which already holds its own stored output.
  const bubbles = mapServerMessagesToPersisted([
    row({ kind: 'tool_call', content: '{}', tool_name: 'shell', tool_call_id: 'a' }),
    row({ kind: 'tool_result', content: null, content_bytes: 1, tool_name: 'shell', tool_call_id: 'a' }),
    row({ kind: 'tool_result', content: null, content_bytes: 2, tool_name: 'shell', tool_call_id: 'zz' }),
  ]);
  assert.equal(bubbles.length, 2);
  assert.equal(bubbles[0]!.toolCall!.storedOutput!.bytes, 1);
  assert.equal(bubbles[1]!.toolCall!.storedOutput!.bytes, 2);
});

// Prompt-mode tool rounds and the trim breadcrumb are stored as `runtime`
// rows: the runtime decides what is machinery, never a string match here.
test('runtime rows (tool rounds, the trim breadcrumb) are not operator bubbles', () => {
  const bubbles = mapServerMessagesToPersisted([
    row({ kind: 'runtime', content: '[earlier turns omitted to fit the context window]' }),
    row({ kind: 'user', content: 'first' }),
    row({ kind: 'runtime', content: '[Tool results]\n<tool_result>r</tool_result>' }),
    row({ kind: 'assistant', content: 'done' }),
    row({ kind: 'user', content: '[Tool results] is what the log says' }),
  ]);
  assert.deepEqual(
    bubbles.map((b) => `${b.role}:${b.content}`),
    ['user:first', 'agent:done', 'user:[Tool results] is what the log says'],
  );
});

test('rows without a stored timestamp get ordered synthetic ones', () => {
  const bubbles = mapServerMessagesToPersisted([
    row({ kind: 'user', content: 'a' }),
    row({ kind: 'assistant', content: 'b' }),
  ]);
  const [first, second] = bubbles.map((b) => Date.parse(b.timestamp));
  assert.ok(first! < second!);
  assert.ok(second! <= Date.now());
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
