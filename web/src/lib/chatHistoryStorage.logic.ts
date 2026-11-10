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
  timestamp: string;
}

function toolResultOutput(value: unknown): string {
  if (typeof value === 'string') return value;
  if (value == null) return '';
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function parseToolArgs(raw: unknown): unknown {
  if (typeof raw !== 'string') return raw ?? {};
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

/**
 * Native dispatcher persists AssistantToolCalls as a JSON assistant row:
 * `{"content": "...", "tool_calls": [{id,name,arguments}], ...}`.
 * Returns null when the row is ordinary assistant text.
 */
export function parseNativeAssistantToolPayload(content: string): {
  text: string;
  thinking?: string;
  toolCalls: Array<{ id?: string; name: string; args?: unknown }>;
} | null {
  const trimmed = content.trim();
  if (!trimmed.startsWith('{')) return null;
  try {
    const parsed = JSON.parse(trimmed) as Record<string, unknown>;
    if (!parsed || typeof parsed !== 'object' || !Array.isArray(parsed.tool_calls)) {
      return null;
    }
    const text =
      typeof parsed.content === 'string'
        ? parsed.content
        : parsed.content == null
          ? ''
          : toolResultOutput(parsed.content);
    const thinking =
      typeof parsed.reasoning_content === 'string' ? parsed.reasoning_content : undefined;
    const toolCalls: Array<{ id?: string; name: string; args?: unknown }> = [];
    for (const raw of parsed.tool_calls) {
      if (!raw || typeof raw !== 'object') continue;
      const tc = raw as Record<string, unknown>;
      const name = typeof tc.name === 'string' ? tc.name : '';
      if (!name) continue;
      toolCalls.push({
        id: typeof tc.id === 'string' ? tc.id : undefined,
        name,
        args: parseToolArgs(tc.arguments),
      });
    }
    return { text, thinking, toolCalls };
  } catch {
    return null;
  }
}

/**
 * Native ToolResults rows are `role: tool` with
 * `{"tool_call_id":"...","content":"..."}`. Non-JSON carriers fall back to
 * the raw string as output.
 */
export function parseNativeToolResultEnvelope(content: string): {
  toolCallId?: string;
  output: string;
} {
  const trimmed = content.trim();
  if (!trimmed.startsWith('{')) {
    return { output: content };
  }
  try {
    const parsed = JSON.parse(trimmed) as Record<string, unknown>;
    if (!parsed || typeof parsed !== 'object') {
      return { output: content };
    }
    const hasEnvelope = 'tool_call_id' in parsed || 'content' in parsed;
    if (!hasEnvelope) {
      return { output: content };
    }
    return {
      toolCallId: typeof parsed.tool_call_id === 'string' ? parsed.tool_call_id : undefined,
      output: toolResultOutput(parsed.content),
    };
  } catch {
    return { output: content };
  }
}

/** Map server-persisted rows into UI messages (timestamps are synthetic for ordering). */
export function mapServerMessagesToPersisted(rows: SessionMessageRow[]): PersistedChatBubble[] {
  // Placeholder timestamp — rewritten after expand so base uses bubble count,
  // not row count (one assistant row can become text + N tool cards).
  const PLACEHOLDER_TS = new Date(0).toISOString();
  const out: PersistedChatBubble[] = [];

  const push = (bubble: Omit<PersistedChatBubble, 'timestamp'>) => {
    out.push({ ...bubble, timestamp: PLACEHOLDER_TS });
  };

  for (const row of rows) {
    if (row.role === 'system') continue;

    if (row.role === 'user') {
      // Machinery the runtime wrote, not operator input: rendering it as a user
      // bubble would show every prompt-mode tool round, and the trim
      // breadcrumb, as if the operator had typed it.
      if (row.synthetic) continue;
      push({
        id: generateUUID(),
        role: 'user',
        content: row.content,
      });
      continue;
    }

    if (row.role === 'assistant') {
      const native = parseNativeAssistantToolPayload(row.content);
      if (native) {
        if (native.text.trim()) {
          push({
            id: generateUUID(),
            role: 'agent',
            content: native.text,
            thinking: native.thinking,
            markdown: true,
          });
        } else if (native.thinking) {
          push({
            id: generateUUID(),
            role: 'agent',
            content: '',
            thinking: native.thinking,
            markdown: true,
          });
        }
        for (const tc of native.toolCalls) {
          push({
            id: generateUUID(),
            role: 'agent',
            content: '',
            toolCall: {
              name: tc.name,
              args: tc.args,
              id: tc.id,
            },
          });
        }
        continue;
      }
      push({
        id: generateUUID(),
        role: 'agent',
        content: row.content,
        markdown: true,
      });
      continue;
    }

    if (row.role === 'tool') {
      const { toolCallId, output } = parseNativeToolResultEnvelope(row.content);
      const matchIdx = resolveToolResultIndex(out, toolCallId);
      if (matchIdx >= 0) {
        const existing = out[matchIdx]!;
        out[matchIdx] = {
          ...existing,
          toolCall: {
            ...existing.toolCall!,
            output,
          },
        };
      } else {
        // Orphan tool result (assistant tool_calls row missing) — still a card,
        // never a raw JSON bubble.
        push({
          id: generateUUID(),
          role: 'agent',
          content: '',
          toolCall: {
            name: 'tool',
            id: toolCallId,
            output,
          },
        });
      }
      continue;
    }

    // Unknown roles: keep as plain agent text (legacy / future carriers).
    push({
      id: generateUUID(),
      role: 'agent',
      content: row.content,
      markdown: false,
    });
  }

  // Stamp after expand: last bubble ≈ now - 1s, never in the future.
  const base = Date.now() - Math.max(out.length, 1) * 1000;
  for (let i = 0; i < out.length; i++) {
    out[i]!.timestamp = new Date(base + i * 1000).toISOString();
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
      timestamp: m.timestamp.toISOString(),
    }));
}
