import type { SessionMessageRow } from '../types/api.ts';
import { generateUUID } from './uuid.ts';
import { resolveToolResultIndex } from './toolCardMatch.ts';
import type { ToolCall } from './toolCall.ts';
export type { ToolCall } from './toolCall.ts';

export interface PersistedChatBubble {
  id: string;
  role: 'user' | 'agent';
  content: string;
  thinking?: string;
  markdown?: boolean;
  /** Verbatim locally-composed user input — never gateway-prefixed, so the
   *  bubble skips stripServerTimestamp for it. (Server rows omit this.) */
  local?: boolean;
  toolCall?: ToolCall;
  /** Server turn number the bubble belongs to, when known. */
  turn?: number;
  timestamp: string;
}

function parseToolArgs(raw: string | null): unknown {
  if (raw == null) return {};
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

/** Map a page of server rows into bubbles: chat rows become bubbles, tool
 *  rows become cards (a result fills its call's card), runtime rows (tool
 *  rounds, the trim breadcrumb) are machinery and are skipped. */
export function mapServerMessagesToPersisted(rows: SessionMessageRow[]): PersistedChatBubble[] {
  const out: PersistedChatBubble[] = [];
  // Rows without a stored timestamp (JSONL) get synthetic, ordered ones below.
  const stamped: boolean[] = [];

  const push = (bubble: Omit<PersistedChatBubble, 'timestamp'>, row: SessionMessageRow) => {
    out.push({ ...bubble, turn: row.turn, timestamp: row.created_at ?? '' });
    stamped.push(row.created_at != null);
  };

  for (const row of rows) {
    switch (row.kind) {
      case 'runtime':
        continue;
      case 'user':
        push({ id: generateUUID(), role: 'user', content: row.content ?? '' }, row);
        continue;
      case 'assistant':
      case 'step':
        push(
          { id: generateUUID(), role: 'agent', content: row.content ?? '', markdown: true },
          row,
        );
        continue;
      case 'tool_call':
        push(
          {
            id: generateUUID(),
            role: 'agent',
            content: '',
            toolCall: {
              name: row.tool_name ?? 'tool',
              args: parseToolArgs(row.content),
              id: row.tool_call_id ?? undefined,
            },
          },
          row,
        );
        continue;
      case 'tool_result': {
        const storedOutput = { messageId: row.id, bytes: row.content_bytes };
        const output = row.content ?? undefined;
        const matchIdx = resolveToolResultIndex(out, row.tool_call_id ?? undefined);
        if (matchIdx >= 0) {
          const existing = out[matchIdx]!;
          out[matchIdx] = {
            ...existing,
            toolCall: { ...existing.toolCall!, storedOutput, output },
          };
        } else {
          // Orphan result (its call row is missing) — still a card.
          push(
            {
              id: generateUUID(),
              role: 'agent',
              content: '',
              toolCall: {
                name: row.tool_name ?? 'tool',
                id: row.tool_call_id ?? undefined,
                storedOutput,
                output,
              },
            },
            row,
          );
        }
        continue;
      }
    }
  }

  // Unstamped bubbles: last ≈ now - 1s, one second apart, never in the future.
  const base = Date.now() - Math.max(out.length, 1) * 1000;
  for (let i = 0; i < out.length; i++) {
    if (!stamped[i]) out[i]!.timestamp = new Date(base + i * 1000).toISOString();
  }
  return out;
}

export function persistedToUiMessages(
  rows: PersistedChatBubble[],
): Array<{
  id: string;
  role: 'user' | 'agent';
  content: string;
  thinking?: string;
  markdown?: boolean;
  local?: boolean;
  toolCall?: ToolCall;
  turn?: number;
  timestamp: Date;
}> {
  return rows.map((m) => ({
    id: m.id,
    role: m.role,
    content: m.content,
    thinking: m.thinking,
    markdown: m.markdown,
    local: m.local,
    toolCall: m.toolCall,
    turn: m.turn,
    timestamp: new Date(m.timestamp),
  }));
}

export function uiMessagesToPersisted(
  messages: Array<{
    id: string;
    role: 'user' | 'agent';
    content: string;
    thinking?: string;
    markdown?: boolean;
    local?: boolean;
    ephemeral?: boolean;
    toolCall?: ToolCall;
    turn?: number;
    timestamp: Date;
  }>,
): PersistedChatBubble[] {
  return messages
    // Skip messages flagged `ephemeral: true` (web slash-command output like
    // /help, /model banners, unknown-command notices). They are throwaway UI
    // feedback and must not be re-hydrated as fake assistant replies on reload. #7137
    // Also skip trajectory carriers (empty content, no reasoning, no card): their
    // only payload is the unpersisted `segments`, so they would reload as an
    // empty bubble.
    .filter((m) => !m.ephemeral && (m.content !== '' || m.thinking || m.toolCall))
    .map((m) => ({
      id: m.id,
      role: m.role,
      content: m.content,
      thinking: m.thinking,
      markdown: m.markdown,
      // Preserve the verbatim-user-input flag so reloaded bubbles still skip
      // server-timestamp stripping.
      local: m.local,
      toolCall: m.toolCall,
      turn: m.turn,
      timestamp: m.timestamp.toISOString(),
    }));
}
