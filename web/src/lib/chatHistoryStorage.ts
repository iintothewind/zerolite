import type { PersistedChatBubble } from './chatHistoryStorage.logic';

export type {
  PersistedChatBubble,
  ToolCall,
} from './chatHistoryStorage.logic';

export {
  mapServerMessagesToPersisted,
  persistedToUiMessages,
  uiMessagesToPersisted,
} from './chatHistoryStorage.logic';

// Expanded tool cards are separate bubbles (1 turn ≈ user + text + N cards).
// 300 keeps local cache depth closer to the pre-expand ~dozens-of-turns window.
const MAX_MESSAGES = 300;
const PREFIX = 'zeroclaw_chat_history_v1:';

function storageKey(sessionId: string): string {
  return `${PREFIX}${sessionId}`;
}

export function loadChatHistory(sessionId: string): PersistedChatBubble[] {
  try {
    const raw = localStorage.getItem(storageKey(sessionId));
    if (!raw) return [];
    const parsed = JSON.parse(raw) as { messages?: PersistedChatBubble[] };
    if (!parsed.messages?.length) return [];
    return parsed.messages;
  } catch {
    return [];
  }
}

export function saveChatHistory(sessionId: string, messages: PersistedChatBubble[]): void {
  try {
    const slice = messages.slice(-MAX_MESSAGES);
    localStorage.setItem(storageKey(sessionId), JSON.stringify({ messages: slice }));
  } catch {
    // QuotaExceeded or private mode
  }
}

/**
 * Drop a conversation's cached transcript.
 *
 * Deleting a conversation has to take the local copy with it: the cache is
 * readable in the browser long after the gateway row is gone, which is not what
 * "delete" promises, and one key per conversation would otherwise accumulate
 * for as long as the origin's storage lives.
 */
export function removeChatHistory(sessionId: string): void {
  try {
    localStorage.removeItem(storageKey(sessionId));
  } catch {
    // Private mode — nothing was cached to begin with.
  }
}
