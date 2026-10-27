import { createContext, useContext, useEffect, useRef, useState, useCallback, useMemo } from 'react';
import type {
  ApprovalDecision,
  PendingApproval,
  SessionMessagesResponse,
  WsMessage,
} from '@/types/api';
import { WebSocketClient } from '@/lib/ws';
import { getActiveSessionId, newSessionId, setActiveSessionId } from '@/lib/chatSessions';
import { generateUUID } from '@/lib/uuid';
import { t } from '@/lib/i18n';
import {
  ApiError,
  HttpError,
  getProp,
  putProp,
  resolveAliasSource,
  listProps,
  getStatus,
  getSessionMessages,
  abortSession,
  deleteSession,
  renameSession,
} from '@/lib/api';
import { primeModelProviderCatalog, modelProviderDisplayName } from '@/lib/modelProviders';
import { resolveAvailableModels } from './modelPicker.logic';
import { contextLimitsFromDoneFrame } from './contextLimits.logic';
import { meterUpdateFromFrame, type ContextInputSource } from './contextMeter.logic';
import { buildHistoryTrimmedNotice } from './historyTrimNotices.logic';
import type { ToolCall } from '@/lib/toolCall';
import { emptyLiveTurn, type LiveTurn, type TurnSegments } from '@/lib/turnSegments';
import { resolveToolResultIndex } from '@/lib/toolCardMatch';
import {
  initialTurnStreamState,
  reduceTurnFrame,
  streamedText,
  type TurnStreamFrame,
  type TurnStreamState,
} from '@/contexts/turnStream.logic';
import {
  applyDone,
  applyUsage,
  displayStats,
  emptyTokenStats,
  turnStarted,
  type LiveStats,
  type TokenStats,
} from '@/lib/sessionStats.logic';
import {
  loadChatHistory,
  mapServerMessagesToPersisted,
  persistedToUiMessages,
  removeChatHistory,
  saveChatHistory,
  uiMessagesToPersisted,
} from '@/lib/chatHistoryStorage';

export interface ChatMessage {
  id: string;
  role: 'user' | 'agent';
  content: string;
  thinking?: string;
  markdown?: boolean;
  toolCall?: ToolCall;
  timestamp: Date;
  /** True for messages composed locally in the web UI (verbatim user input).
   *  Such content never carries the gateway's `[timestamp]` prefix, so the
   *  bubble must NOT run stripServerTimestamp on it — otherwise a user message
   *  that happens to start with a bracketed datetime would be clipped. Only
   *  server-sourced messages (live stream + hydrated history) can be prefixed. */
  local?: boolean;
  /**
   * Locally-generated info/system message produced by web slash-command
   * handlers (`/help`, `/model`, unknown-command notices). Excluded from
   * persistence so command output does not pollute localStorage and reappear
   * as fake assistant replies on reload. See #7137.
   */
  ephemeral?: boolean;
  /** User-visible lifecycle notice, rendered distinctly from agent output. */
  notice?: boolean;
  /**
   * The turn's step trajectory, attached when a turn commits. Live-only: it is
   * built from the WebSocket stream, deliberately not persisted (see
   * `docs/reports/webui-message-flow-plan.md`), and dropped by
   * `uiMessagesToPersisted` — which is what makes a reloaded conversation
   * render ungrouped. Absent on every hydrated message.
   */
  segments?: TurnSegments;
}

/**
 * Keep the newest `keptTurns` user-led exchanges — a user bubble starts a turn,
 * which is how the runtime counts `kept_turns`. Contract: `TurnEvent::HistoryTrimmed`
 * (`crates/zeroclaw-api/src/agent.rs`).
 */
export function purgeUiMessagesToKeptUserTurns(
  messages: ChatMessage[],
  keptTurns: number,
): ChatMessage[] {
  if (keptTurns <= 0 || messages.length === 0) {
    return messages;
  }
  const userIndexes: number[] = [];
  for (let i = 0; i < messages.length; i++) {
    if (messages[i]?.role === 'user') {
      userIndexes.push(i);
    }
  }
  if (userIndexes.length <= keptTurns) {
    return messages;
  }
  const start = userIndexes[userIndexes.length - keptTurns];
  if (start === undefined) {
    return messages;
  }
  return messages.slice(start);
}

export interface AgentContextValue {
  messages: ChatMessage[];
  sendMessage: (content: string) => void;
  connected: boolean;
  error: string | null;
  typing: boolean;
  currentModel: string | null;
  availableModels: string[];
  switchModel: (model: string) => Promise<void>;
  modelLoading: boolean;
  /** Re-fetch model list from server. Useful after user edits config externally. */
  refreshModels: () => void;
  deleteMessage: (id: string) => void;
  clearAllMessages: () => void;
  /** Conversation currently on screen for this agent (issue #7543). */
  sessionId: string;
  /**
   * True once the active conversation's transcript has loaded. Sending before
   * this would race the hydration fetch, whose all-or-nothing mutation fence
   * then drops the fetched history entirely — so the composer waits on it.
   */
  hydrated: boolean;
  /**
   * Whether the gateway stores conversations; null before the first answer.
   * False means only the live socket holds context, so there is nothing to
   * switch back to and session management is withheld.
   */
  sessionPersistence: boolean | null;
  /** Conversations another pane is showing; not selectable or deletable here. */
  reservedSessionIds: readonly string[];
  /** Begin an additional conversation without discarding the current one. */
  startNewSession: () => boolean;
  /** Resume a stored conversation; its transcript is rehydrated. */
  goToSession: (sessionId: string) => boolean;
  /** Delete a stored conversation, moving off it when it is the active one. */
  removeSession: (sessionId: string) => Promise<void>;
  /** Persist the operator's explicit conversation name. */
  renameConversation: (sessionId: string, name: string) => Promise<void>;
  /**
   * Append a locally-generated info/system message to the transcript without
   * sending anything to the gateway. Used by web slash-command handlers
   * (`/help`, `/model`, unknown-command notices) to surface feedback inline.
   * See #7137.
   */
  addLocalMessage: (content: string) => void;
  abortSession: () => Promise<void>;
  /**
   * Pending supervised-mode tool-approval prompt, or null. Populated when the
   * gateway emits an `approval_request` frame; cleared once the user responds
   * or a fresh `approval_request` arrives. See #6522.
   */
  pendingApproval: PendingApproval | null;
  respondToApproval: (decision: ApprovalDecision) => void;
  // Context window tracking (from "done" WS frames). See #7311 and
  // docs/book/src/agents/context-management.md.
  /** The effective context window: the meter denominator. */
  contextEffectiveWindow: number | null;
  /** The trim budget: where proactive trimming engages, and what it retains to. */
  contextTrimBudgetTokens: number | null;
  contextInputTokens: number | null;
  /** Provenance of `contextInputTokens` — the label the ring panel shows.
   * `calibrated` means the number is a provider measurement anchored on the
   * unchanged prefix plus a local estimate for the unbilled tail. */
  contextInputSource: ContextInputSource;
  /** Composer stats. See `webui-overall-plan.md` §10 "Composer stats semantics". */
  liveStats: LiveStats;
  /** The turn in flight: its closed steps and the step still being written.
   *  Empty between turns, and the transcript renders it as the live trajectory
   *  group plus the answer still arriving. */
  liveTurn: LiveTurn;
}

const AgentContext = createContext<AgentContextValue | null>(null);

export function useAgent() {
  const ctx = useContext(AgentContext);
  if (!ctx) throw new Error('useAgent must be used within AgentProvider');
  return ctx;
}

const MODEL_SWITCH_TIMEOUT_MS = 10_000;

function friendlyAgentError(message?: string): string {
  const raw = message?.trim() || t('agent.unknown_error');
  const localConnectFailure = raw.match(
    /model_provider=(\w+)\s+model=([^\s]+).*?url \((https?:\/\/[^)]+)\).*?(?:Connection refused|tcp connect error)/i,
  );
  if (localConnectFailure) {
    const provider = localConnectFailure[1] ?? '';
    const model = localConnectFailure[2] ?? 'the selected model';
    const url = localConnectFailure[3] ?? 'the configured endpoint';
    const displayProvider = modelProviderDisplayName(provider);
    return `${displayProvider} is unreachable at ${url}. Start the local provider service, confirm it serves ${model}, then try again.`;
  }
  return raw;
}

export interface AgentProviderProps {
  /** Configured agent alias this provider is bound to. The WebSocket
   * connection, session ID, and chat history are all scoped to this alias. */
  agentAlias: string;
  /**
   * Conversation to open. Omit to resume the alias's last one. The workspace
   * passes it per pane so the same agent can be open twice on two different
   * conversations, which the per-alias pointer alone cannot express.
   */
  initialSessionId?: string;
  /**
   * Conversations already claimed by other panes. Those must not be opened or
   * deleted from here: two sockets on one gateway session produce two Agents
   * whose histories diverge, and abort/clear address a session by id, so the
   * panes would cancel and wipe each other's turns.
   */
  reservedSessionIds?: readonly string[];
  /** Called when this pane moves to another conversation, so the owner can
   * remember where the pane is. */
  onSessionChange?: (sessionId: string) => void;
  children: React.ReactNode;
  /** Narrow session boundary used by the headless lifecycle regression suite. */
  sessionRuntime?: AgentSessionRuntime;
}

export interface SessionSocket {
  onMessage: ((msg: WsMessage) => void) | null;
  onOpen: (() => void) | null;
  onClose: ((ev: CloseEvent) => void) | null;
  onError: ((ev: Event) => void) | null;
  connect(): void;
  disconnect(): void;
  readonly connected: boolean;
  sendMessage(content: string): void;
  sendApprovalResponse(requestId: string, decision: ApprovalDecision): void;
}

export interface AgentSessionRuntime {
  createSocket(options: { agentAlias: string; sessionId: string }): SessionSocket;
  getMessages(sessionId: string): Promise<SessionMessagesResponse>;
  delete(sessionId: string): Promise<{ deleted: boolean }>;
  rename(sessionId: string, name: string): Promise<{ session_id: string; name: string }>;
  mintId(): string;
}

const defaultSessionRuntime: AgentSessionRuntime = {
  createSocket: (options) => new WebSocketClient(options),
  getMessages: getSessionMessages,
  delete: deleteSession,
  rename: renameSession,
  mintId: newSessionId,
};

/** Shared empty array so panes with no siblings keep a stable context value. */
const NO_RESERVED_SESSIONS: readonly string[] = [];

export function AgentProvider({
  agentAlias,
  initialSessionId,
  reservedSessionIds,
  onSessionChange,
  children,
  sessionRuntime = defaultSessionRuntime,
}: AgentProviderProps) {
  // The conversation this provider is showing. State, not a ref: switching it
  // is what re-runs hydration and rebuilds the socket, which is how one agent
  // serves several independent conversations (issue #7543).
  const [sessionId, setSessionId] = useState(
    () => initialSessionId ?? getActiveSessionId(agentAlias),
  );
  const [messages, setMessages] = useState<ChatMessage[]>(() => {
    const persisted = loadChatHistory(sessionId);
    return persisted.length > 0 ? persistedToUiMessages(persisted) : [];
  });
  // Session whose transcript has finished hydrating into `messages`. Null until
  // the first hydration lands, and never equal to `sessionId` while a switch is
  // in flight — which is what stops the mirror effect below from writing one
  // conversation's transcript under another conversation's key.
  const [hydratedSessionId, setHydratedSessionId] = useState<string | null>(null);
  // Whether the gateway stores transcripts, as reported by the hydration fetch.
  // Null until the first answer. When false there is no server-side record of a
  // conversation, so switching away from one strands it — the picker uses this
  // to withhold the controls that would do that.
  const [sessionPersistence, setSessionPersistence] = useState<boolean | null>(null);
  const [connected, setConnected] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [typing, setTyping] = useState(false);
  const [currentModel, setCurrentModel] = useState<string | null>(null);
  const [availableModels, setAvailableModels] = useState<string[]>([]);
  const [modelLoading, setModelLoading] = useState(false);
  const [modelInfoVersion, setModelInfoVersion] = useState(0);
  const [pendingApproval, setPendingApproval] = useState<PendingApproval | null>(null);
  // Context window tracking (from "done" WS frames). See #7311.
  const [contextEffectiveWindow, setContextEffectiveWindow] = useState<number | null>(null);
  const [contextTrimBudgetTokens, setContextTrimBudgetTokens] = useState<number | null>(null);
  const [contextInputTokens, setContextInputTokens] = useState<number | null>(null);
  const [contextInputSource, setContextInputSource] = useState<ContextInputSource>(null);
  const [tokenStats, setTokenStats] = useState<TokenStats>(emptyTokenStats);
  // Mirrored from the turn-stream ref for the live trajectory view.
  const [liveTurn, setLiveTurn] = useState<LiveTurn>(emptyLiveTurn);
  const liveStats = useMemo(
    () => displayStats(messages, tokenStats, liveTurn),
    [messages, tokenStats, liveTurn],
  );

  const wsRef = useRef<WebSocketClient | null>(null);
  // Canonical per-turn stream state. Every production transition that mutates
  // it goes through reduceTurnFrame, which is exercised directly by the
  // frame-sequence regression suite.
  const turnStreamStateRef = useRef<TurnStreamState>(initialTurnStreamState());
  const pendingModelSwitchRef = useRef<string | null>(null);
  // A session navigation can open another socket while the config PUT is in
  // flight. Only an open that happens after the write committed proves the
  // live Agent was constructed with the requested model.
  const modelSwitchConfigCommittedRef = useRef(false);
  const modelSwitchSocketRef = useRef<SessionSocket | null>(null);
  const switchTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const wsVersionRef = useRef(0);
  // Socket generation that delivered the live `approval_request`, or null.
  // A parked approval dies with the socket that carried it (the gateway
  // auto-denies the request_id when that socket closes), so only that socket's
  // close may clear the banner; a stale close must not wipe an approval a
  // newer socket owns. See the onClose handler in attachSocketCallbacks.
  const approvalSocketVersionRef = useRef<number | null>(null);
  const localMessageMutationVersionRef = useRef(0);
  // Rebuild callbacks intentionally retain the dependency shape already on
  // master. These mirrors still make their async work use the latest session
  // and injected runtime after a conversation switch.
  const activeSessionIdRef = useRef(sessionId);
  const sessionPersistenceRef = useRef(sessionPersistence);
  const typingRef = useRef(typing);
  const sessionRuntimeRef = useRef(sessionRuntime);
  activeSessionIdRef.current = sessionId;
  sessionPersistenceRef.current = sessionPersistence;
  typingRef.current = typing;
  sessionRuntimeRef.current = sessionRuntime;

  // Prime the model-provider catalog once so error formatting can resolve
  // display names from the backend registry rather than a local shadow list.
  useEffect(() => {
    void primeModelProviderCatalog();
  }, []);

  // Hydrate chat from server (preferred) or localStorage fallback. Re-runs on
  // every session switch, which is what rehydrates the transcript of the
  // conversation the operator just selected.
  useEffect(() => {
    const sid = sessionId;
    const hydrationStartedAtMutationVersion = localMessageMutationVersionRef.current;
    let cancelled = false;

    // Session management is a capability, not an optimistic default. Every
    // hydration starts unknown and only an affirmative gateway response opens
    // the transition actions.
    setSessionPersistence(null);
    setHydratedSessionId(null);

    (async () => {
      try {
        const res = await sessionRuntime.getMessages(sid);
        if (cancelled) return;
        setSessionPersistence(res.session_persistence);
        if (res.session_persistence) {
          if (localMessageMutationVersionRef.current === hydrationStartedAtMutationVersion) {
            setMessages(persistedToUiMessages(mapServerMessagesToPersisted(res.messages)));
          }
        } else if (!res.session_persistence) {
          setMessages((prev) => {
            if (prev.length > 0) return prev;
            const ls = loadChatHistory(sid);
            return ls.length ? persistedToUiMessages(ls) : prev;
          });
        }
      } catch {
        if (!cancelled) {
          setSessionPersistence(null);
          setMessages((prev) => {
            if (prev.length > 0) return prev;
            const ls = loadChatHistory(sid);
            return ls.length ? persistedToUiMessages(ls) : prev;
          });
        }
      } finally {
        if (!cancelled) setHydratedSessionId(sid);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [sessionId, sessionRuntime]);

  // Mirror transcript to localStorage (bounded); server remains source of truth when persistence is on
  useEffect(() => {
    if (hydratedSessionId !== sessionId) return;
    saveChatHistory(sessionId, uiMessagesToPersisted(messages));
  }, [messages, hydratedSessionId, sessionId]);

  // Auto-clear a pending approval when its timeout elapses on the backend.
  // The gateway auto-denies after `timeout_secs`; without this effect the
  // banner would linger indefinitely if the user just walked away. Add a
  // small grace buffer so the user is not penalised for last-second clicks.
  useEffect(() => {
    if (!pendingApproval) return;
    const elapsed = Date.now() - pendingApproval.receivedAt;
    const remainingMs = pendingApproval.timeoutSecs * 1000 - elapsed + 500;
    if (remainingMs <= 0) {
      setPendingApproval(null);
      return;
    }
    const id = setTimeout(() => {
      setPendingApproval((current) =>
        current && current.requestId === pendingApproval.requestId ? null : current,
      );
    }, remainingMs);
    return () => clearTimeout(id);
  }, [pendingApproval]);

  const foldTurnStream = useCallback((frame: TurnStreamFrame) => {
    const result = reduceTurnFrame(turnStreamStateRef.current, frame);
    turnStreamStateRef.current = result.state;
    // Mirror the trajectory out for the streaming view. A terminal frame hands
    // back fresh state, so this empties the live view exactly when the
    // committed message takes over.
    setLiveTurn({ steps: result.state.segments.steps, open: result.state.openStep });
    return result;
  }, []);

  // Centralised WebSocket message handler — reused across initial connect and reconnects.
  const handleWsMessage = useCallback((msg: WsMessage) => {
    // One place folds the context meter, so every frame that prices the request
    // — the trim's post-trim size, the assembled estimate at turn start, the
    // provider's measurement, the terminal size — moves the ring the same way.
    const meterUpdate = meterUpdateFromFrame(msg);
    if (meterUpdate) {
      setContextInputTokens(meterUpdate.used);
      if (meterUpdate.source !== undefined) setContextInputSource(meterUpdate.source);
    }
    switch (msg.type) {
      case 'session_start':
      case 'connected':
        break;

      case 'agent_start':
        // Turn boundary for token bookkeeping (`applyDone` reconciliation).
        // Displayed turns come from user bubbles in `messages`, not this frame.
        setTokenStats(turnStarted);
        break;

      case 'usage':
        // One LLM call completed: its response is accepted, which is the
        // message-flow view's segment boundary. Folding here — rather than
        // letting the next `tool_call` close the step — is what keeps a
        // tool-free step from merging into the one after it, and what makes
        // several calls arriving together one step rather than several.
        foldTurnStream({ type: 'usage' });
        setTokenStats((s) => applyUsage(s, msg));
        // (The meter moves through `meterUpdateFromFrame` above.)
        break;

      case 'context_usage':
        // Pre-dispatch assembled size, published once per turn. Deliberately
        // not folded as a step: the fold only listens to `usage`, because this
        // frame means "a request is about to go out", not "a call completed".
        break;

      case 'thinking':
        setTyping(true);
        foldTurnStream({ type: 'thinking', content: msg.content });
        break;

      case 'chunk':
        setTyping(true);
        foldTurnStream({ type: 'chunk', content: msg.content });
        break;

      case 'chunk_reset':
        // Server signals that the authoritative done message follows. The
        // trajectory keeps the text — it is the record, not the display — and
        // this frame has no emitter left (see `ws.rs`), so nothing else here.
        foldTurnStream({ type: 'chunk_reset' });
        break;

      case 'message':
      case 'done': {
        const { completion: outcome, segments } = foldTurnStream({
          type: msg.type,
          full_response: msg.full_response,
          content: msg.content,
        });
        // A trajectory worth rendering is one that actually has steps; an empty
        // group would show a header reading `0 次工具调用`.
        const trajectory = segments.steps.length > 0 ? segments : undefined;
        if (outcome?.kind === 'commit') {
          // `commit` includes reasoning-only turns: empty content but present
          // thinking, so the turn renders instead of vanishing silently.
          localMessageMutationVersionRef.current += 1;
          setMessages((prev) => [
            ...prev,
            {
              id: generateUUID(),
              role: 'agent',
              content: outcome.content,
              thinking: outcome.thinking,
              markdown: true,
              timestamp: new Date(),
              segments: trajectory,
            },
          ]);
        } else if (outcome?.kind === 'diagnostic') {
          // Clean completion with nothing at all — surface a one-off notice so
          // the turn does not disappear. Mirrors zerocode's zc-turn-no-output
          // fallback (#8779). `skip` (empty + tool calls ran) renders nothing.
          localMessageMutationVersionRef.current += 1;
          setMessages((prev) => [
            ...prev,
            {
              id: generateUUID(),
              role: 'agent',
              content: t('agent.turn_no_output'),
              timestamp: new Date(),
              ephemeral: true,
            },
          ]);
        }
        // Extract context window info from "done" frame (sent by gateway).
        if (msg.type === 'done') {
          // Reconcile the finished turn against the gateway's own step count
          // and cache total: the client counts `usage` frames, the gateway
          // counts `Usage` events, and they diverge only when the page missed
          // part of the turn.
          setTokenStats((s) => applyDone(s, msg));
          {
            // One authoritative snapshot per done frame: the effective window
            // (meter denominator) and the trim budget (trigger and fit target).
            const limits = contextLimitsFromDoneFrame(msg);
            setContextEffectiveWindow(limits.effectiveWindow);
            setContextTrimBudgetTokens(limits.trimBudgetTokens);
          }
          // The terminal size reaches the meter through `meterUpdateFromFrame`
          // above, which is also what keeps a trim that landed after the final
          // call from being undone here.
        }
        setTyping(false);
        break;
      }

      case 'tool_call': {
        // Defense in depth (issue #7151): the chat WebSocket shares a broadcast
        // bus with observability telemetry, whose `tool_call` frames have a
        // different shape (`tool`/`duration_ms`/`success`) and carry no `name`.
        // Such a frame would otherwise produce a permanent "unknown" tool card
        // with a spinner that never resolves (no matching `tool_result`). The
        // backend already filters these out, but ignore them here too so a
        // malformed telemetry frame can never render a stuck card.
        if (!msg.name) {
          foldTurnStream({ type: 'tool_call', hasName: false });
          break;
        }
        const toolName = msg.name;
        const toolArgs = msg.args;
        const { state } = foldTurnStream({
          type: 'tool_call',
          hasName: true,
          // The same call, recorded in the turn's step trajectory as well as in
          // the message list. The step that made it was closed by its `usage`
          // frame, so this attaches to that step — several calls here is
          // parallel tool use, not several steps.
          call: { name: toolName, args: toolArgs, id: msg.id },
        });
        localMessageMutationVersionRef.current += 1;
        setMessages((prev) => {
          const argsKey = JSON.stringify(toolArgs ?? {});
          if (streamedText(state)) {
            const isDuplicate = prev.some(
              (m) => m.toolCall
                && m.toolCall.output === undefined
                && m.toolCall.name === toolName
                && JSON.stringify(m.toolCall.args ?? {}) === argsKey,
            );
            if (isDuplicate) return prev;
          }

          return [
            ...prev,
            {
              id: generateUUID(),
              role: 'agent' as const,
              content: `${t('agent.tool_call_prefix')} ${toolName}(${argsKey})`,
              toolCall: { name: toolName, args: toolArgs, id: msg.id },
              timestamp: new Date(),
            },
          ];
        });
        break;
      }

      case 'tool_result': {
        // Defense in depth (issue #7151): mirrors the `tool_call` guard
        // above. Observability `tool_result`-shaped telemetry would otherwise
        // overwrite the most recent pending tool card with an empty output.
        if (!msg.name) {
          break;
        }
        const toolName = msg.name;
        const resultId = msg.id;
        // Fill the matching call inside the step trajectory too, correlating by
        // `tool_call_id` for the same reason the card does: parallel calls can
        // complete out of order.
        foldTurnStream({ type: 'tool_result', id: resultId, output: msg.output ?? '' });
        localMessageMutationVersionRef.current += 1;
        setMessages((prev) => {
          // Correlate the result to its pending card by gateway tool_call_id so
          // out-of-order parallel results land on the right card; see
          // resolveToolResultIndex for the id-less fallback.
          const idx = resolveToolResultIndex(prev, resultId);
          if (idx !== -1) {
            const updated = [...prev];
            const existing = prev[idx]!;
            updated[idx] = {
              ...existing,
              toolCall: { ...existing.toolCall!, output: msg.output ?? '' },
            };
            return updated;
          }
          return [
            ...prev,
            {
              id: generateUUID(),
              role: 'agent' as const,
              content: `${t('agent.tool_result_prefix')} ${msg.output ?? ''}`,
              toolCall: { name: toolName, output: msg.output ?? '' },
              timestamp: new Date(),
            },
          ];
        });
        break;
      }

      case 'cron_result': {
        const cronOutput = msg.output ?? '';
        if (cronOutput) {
          localMessageMutationVersionRef.current += 1;
          setMessages((prev) => [
            ...prev,
            {
              id: generateUUID(),
              role: 'agent' as const,
              content: cronOutput,
              markdown: true,
              timestamp: new Date(msg.timestamp ?? Date.now()),
            },
          ]);
        }
        break;
      }

      case 'history_trimmed': {
        // Token-rich when the daemon sent `tokens_before`/`tokens_after`;
        // falls back to the count-only wording for older daemons and
        // message-limit trims. Rendering lives in `historyTrimNotices.logic.ts`
        // so the wording is unit-testable without a React harness.
        const content = buildHistoryTrimmedNotice(msg, t);
        const notice = {
          id: generateUUID(),
          role: 'agent' as const,
          content,
          timestamp: new Date(),
          ephemeral: true,
          notice: true,
        };
        // Cut locally instead of re-reading the store: a store read at this
        // point returns the pre-trim transcript, and rebuilding the list from
        // server rows drops the client-only `segments` that fold a committed
        // turn. Contract: `history_trimmed_ws_frame`
        // (`crates/zeroclaw-gateway/src/ws.rs`).
        // A trim is server-side state the browser's counters know nothing
        // about, so they must be zeroed here. This is the only reset that is
        // not free. See the composer plan §4.1.
        setTokenStats(emptyTokenStats);
        localMessageMutationVersionRef.current += 1;
        const keptTurns = typeof msg.kept_turns === 'number' ? msg.kept_turns : 0;
        setMessages((prev) => {
          const withoutNotices = prev.filter((m) => !m.notice);
          const purged = purgeUiMessagesToKeptUserTurns(withoutNotices, keptTurns);
          return [...purged, notice];
        });
        break;
      }

      case 'safeguard_fallback': {
        // Display-only safety-safeguard downgrade notice. Mirrors
        // `history_trimmed`: rendered as an ephemeral warning bubble that is
        // never persisted to localStorage or the backend transcript. Privacy:
        // the gateway sends only model names and which layer switched — no
        // classifier category or refusal explanation reaches the browser.
        const served = msg.served_model ?? '';
        const requested = msg.requested_model ?? '';
        if (!served || !requested) break;
        const key = msg.fallback_kind === 'server'
          ? 'agent.safeguard_fallback_server'
          : msg.fallback_kind === 'client_server'
            ? 'agent.safeguard_fallback_client_server'
            : 'agent.safeguard_fallback_client';
        const content = t(key)
          .replace('{requested}', requested)
          .replace('{served}', served);
        localMessageMutationVersionRef.current += 1;
        setMessages((prev) => [
          ...prev,
          {
            id: generateUUID(),
            role: 'agent' as const,
            content,
            timestamp: new Date(),
            ephemeral: true,
            notice: true,
          },
        ]);
        break;
      }

      case 'approval_request': {
        // Supervised-mode tool consent prompt. Backend parks on a oneshot
        // until we send `approval_response`; if the socket closes or the
        // timeout elapses, the backend auto-denies on its side.
        if (!msg.request_id) break;
        setPendingApproval({
          requestId: msg.request_id,
          toolName: msg.tool ?? 'unknown',
          argumentsSummary: msg.arguments_summary ?? '',
          timeoutSecs: msg.timeout_secs ?? 120,
          receivedAt: Date.now(),
        });
        break;
      }

      case 'aborted': {
        // Gateway sends this after a cancelled turn; the parked approval (if
        // any) is no longer valid because its request_id belongs to the old
        // turn. Clear so the banner does not linger across the abort.
        // `aborted` carries the same totals as `done`: a cancelled turn still
        // spent the steps and tokens it spent, so the stats row keeps them.
        setTokenStats((s) => applyDone(s, msg));
        foldTurnStream({ type: 'aborted' });
        setTyping(false);
        setPendingApproval(null);
        break;
      }

      case 'error':
        const friendlyMessage = friendlyAgentError(msg.message);
        localMessageMutationVersionRef.current += 1;
        setMessages((prev) => [
          ...prev,
          {
            id: generateUUID(),
            role: 'agent',
            content: `${t('agent.error_prefix')} ${friendlyMessage}`,
            timestamp: new Date(),
          },
        ]);
        if (msg.code === 'AGENT_INIT_FAILED' || msg.code === 'AUTH_ERROR' || msg.code === 'PROVIDER_ERROR') {
          setError(`${t('agent.configuration_error')}: ${friendlyMessage}`);
        } else if (msg.code === 'INVALID_JSON' || msg.code === 'UNKNOWN_MESSAGE_TYPE' || msg.code === 'EMPTY_CONTENT') {
          setError(`${t('agent.message_error')}: ${msg.message}`);
        }
        setTyping(false);
        foldTurnStream({ type: 'error' });
        setPendingApproval(null);
        break;
    }
  }, [foldTurnStream]);

  // Wire up a WebSocketClient instance with version-guarded callbacks.
  const attachSocketCallbacks = useCallback((ws: SessionSocket) => {
    const version = ++wsVersionRef.current;

    ws.onOpen = () => {
      if (version !== wsVersionRef.current) return;
      setConnected(true);
      setError(null);

      // If we just reconnected after a committed model switch, apply the
      // pending model now. A session socket can open while the config write is
      // still pending; that socket was constructed from the old config and
      // must not settle the switch.
      if (
        pendingModelSwitchRef.current
        && modelSwitchConfigCommittedRef.current
        && modelSwitchSocketRef.current === ws
      ) {
        if (switchTimeoutRef.current) {
          clearTimeout(switchTimeoutRef.current);
          switchTimeoutRef.current = null;
        }
        setCurrentModel(pendingModelSwitchRef.current);
        setModelInfoVersion((v) => v + 1);
        pendingModelSwitchRef.current = null;
        modelSwitchConfigCommittedRef.current = false;
        modelSwitchSocketRef.current = null;
        setModelLoading(false);
      }
    };

    ws.onClose = (ev: CloseEvent) => {
      // A parked approval belongs to the socket that delivered it: the gateway
      // auto-denies that request_id when its socket closes, so the banner must
      // not survive this close even when the socket is already stale. But
      // ownership, not staleness, decides. A session switch disconnects the
      // outgoing socket without detaching this callback, so after A -> B the
      // browser can deliver A's close once B is current and holds its own
      // approval; A owns nothing live any more and must leave B's banner alone.
      if (approvalSocketVersionRef.current === version) {
        approvalSocketVersionRef.current = null;
        setPendingApproval(null);
      }
      if (version !== wsVersionRef.current) return;
      setConnected(false);

      if (
        pendingModelSwitchRef.current
        && modelSwitchConfigCommittedRef.current
        && modelSwitchSocketRef.current === ws
      ) {
        // We intentionally closed the old socket; non-normal codes mean the reconnect failed.
        if (ev.code !== 1000 && ev.code !== 1001) {
          setError(`${t('agent.connection_closed')} (code: ${ev.code}). ${t('agent.check_configuration')}.`);
        }
        pendingModelSwitchRef.current = null;
        modelSwitchConfigCommittedRef.current = false;
        modelSwitchSocketRef.current = null;
        if (switchTimeoutRef.current) {
          clearTimeout(switchTimeoutRef.current);
          switchTimeoutRef.current = null;
        }
        setModelLoading(false);
        return;
      }

      if (ev.code !== 1000 && ev.code !== 1001) {
        setError(`${t('agent.connection_closed')} (code: ${ev.code}). ${t('agent.check_configuration')}.`);
      }
    };

    ws.onError = () => {
      if (version !== wsVersionRef.current) return;
      // During the post-write reconnect we let onClose deliver the final
      // verdict. A socket failure during the config write is still an ordinary
      // connection error and does not make the persisted write disappear.
      if (
        !pendingModelSwitchRef.current
        || !modelSwitchConfigCommittedRef.current
        || modelSwitchSocketRef.current !== ws
      ) {
        setError(t('agent.connection_error'));
      }
    };

    ws.onMessage = (msg: WsMessage) => {
      if (version !== wsVersionRef.current) return;
      if (msg.type === 'approval_request' && msg.request_id) {
        approvalSocketVersionRef.current = version;
      }
      handleWsMessage(msg);
    };
  }, [handleWsMessage]);

  // WebSocket bound to the configured agent and the active conversation.
  // Re-keys (via the outer <AgentProvider key={alias}>) when the alias changes,
  // and re-runs on a session switch so the gateway seeds a fresh Agent from the
  // selected session's persisted history.
  useEffect(() => {
    const ws = sessionRuntime.createSocket({ agentAlias, sessionId });
    // A navigation after the config write creates an equally valid replacement
    // to the one switchModel would build directly. Mark only sockets created
    // after commit; a socket merely opening after commit may still have been
    // constructed by the gateway from the old config.
    if (pendingModelSwitchRef.current && modelSwitchConfigCommittedRef.current) {
      modelSwitchSocketRef.current = ws;
    }
    attachSocketCallbacks(ws);
    ws.connect();
    wsRef.current = ws as WebSocketClient;

    return () => {
      ws.disconnect();
      // switchModel and clearAllMessages replace the socket this effect
      // created. Before sessions were switchable the effect only re-ran on an
      // alias change, so that replacement was never left behind; now a session
      // switch tears the effect down at any time, and closing only the captured
      // socket would strand the replacement — still connected, still streaming
      // into a conversation the UI has left.
      if (wsRef.current && wsRef.current !== ws) {
        wsRef.current.disconnect();
      }
    };
  }, [attachSocketCallbacks, agentAlias, sessionId, sessionRuntime]);

  // Fetch current model and available models from config.
  useEffect(() => {
    let cancelled = false;

    async function loadModelInfo() {
      try {
        // Agent-scoped status: `/api/status?agent=<alias>` runs the same
        // `resolved_model_provider_for_agent` logic the gateway uses to build
        // the Agent, so the fallback `status.model` is correct for THIS agent
        // rather than the install-wide default. (Ported from
        // zeroclaw-labs/zeroclaw#7191.)
        const status = await getStatus(agentAlias);
        if (cancelled) return;

        let activeModel = status.model;

        // Per-agent model (multi-agent / schema V3). The old global `model` /
        // `default_model` keys were removed in V3, so the source of truth is THIS
        // agent's own `agents.<alias>.model_provider` — a ref into
        // `providers.models.<family>.<alias>`. (Previously this read the global
        // keys, which now 404, so the button fell back to the daemon's global
        // status model — the wrong model on every agent page.)
        let activeRef: string | null = null;
        try {
          const refProp = await getProp(`agents.${agentAlias}.model_provider`);
          // NOTE: GET /api/config/prop returns only `{ path, value }` — it has
          // no `populated` field (that exists only on /api/config/list). A set
          // prop has a string value; an unset one returns the "<unset>"
          // sentinel; a missing path throws. So gate on the value, not
          // `populated` (which would be undefined here and silently fail).
          if (typeof refProp.value === 'string' && refProp.value !== '<unset>') {
            activeRef = refProp.value;
          }
        } catch {
          // ignore — fall back to the status value below
        }
        if (cancelled) return;
        // Show the agent's configured provider ref (e.g. "kilo.minimax_m3"),
        // falling back to the daemon status model only if unset.
        setCurrentModel(activeRef ?? activeModel);

        // Available switch targets = every configured provider ref
        // (`providers.models.<family>.<alias>`), resolved and sorted by the
        // config layer's canonical alias-source resolver.
        //
        // The `/api/config/resolve-alias-source` endpoint only exists on
        // daemons that declare the `PropKind::AliasRef` contract. Embedded-web
        // builds ship the bundle inside the daemon binary, so skew is
        // impossible there — but `gateway.web_dist_dir` filesystem serving can
        // pair a newer bundle with an older daemon. `resolveAvailableModels`
        // falls back to the pre-resolver `listProps` scan (sorted through the
        // same `family.alias` key) when the endpoint is unavailable, so the
        // picker never silently collapses to a single ref on those deployments.
        try {
          const refs = await resolveAvailableModels({
            resolveAliasSource: () => resolveAliasSource('model_providers'),
            listProps: () => listProps('providers.models'),
          });
          if (cancelled) return;
          setAvailableModels(refs.length > 0 ? refs : activeRef ? [activeRef] : []);
        } catch {
          setAvailableModels(activeRef ? [activeRef] : []);
        }
      } catch {
        // Ignore errors — dropdown will just show current model once loaded
      }
    }

    loadModelInfo();

    return () => {
      cancelled = true;
    };
  }, [modelInfoVersion, agentAlias]);

  const sendMessage = useCallback((content: string) => {
    if (!wsRef.current?.connected) return;
    try {
      wsRef.current.sendMessage(content);

      setTyping(true);
      foldTurnStream({ type: 'turn_start' });
      localMessageMutationVersionRef.current += 1;
      setMessages((prev) => [
        ...prev,
        {
          id: generateUUID(),
          role: 'user',
          content,
          timestamp: new Date(),
          local: true,
        },
      ]);
    } catch {
      setError(t('agent.send_error'));
    }
  }, [foldTurnStream]);

  const switchModel = useCallback(async (model: string) => {
    if (modelLoading) return; // debounce
    setModelLoading(true);
    pendingModelSwitchRef.current = model;
    modelSwitchConfigCommittedRef.current = false;
    modelSwitchSocketRef.current = null;

    // Watchdog so the UI can never get stuck on the loading spinner. It is
    // armed once per phase — for the config write, then again for the socket
    // reconnect — so each phase gets its own full budget. A single timer armed
    // at the top had to cover *both* phases: a slow daemon write could consume
    // the whole budget and fire "model switch timed out" while the switch was
    // still progressing (and, because it nulled the pending ref, the later
    // onOpen would skip updating currentModel — a timeout error for a switch
    // that actually succeeded). Splitting the budget keeps the spinner bounded
    // against a hung request *and* a reconnect that never opens, without the
    // false positive. The `=== model` identity check stops a fired watchdog
    // from clobbering a newer switch. (Ported from zeroclaw-labs/zeroclaw#7191.)
    const armWatchdog = () => {
      if (switchTimeoutRef.current) clearTimeout(switchTimeoutRef.current);
      switchTimeoutRef.current = setTimeout(() => {
        // The timer has fired; null its ref so it honestly reflects
        // "no watchdog armed" (a later clearTimeout on the dead id is a no-op).
        switchTimeoutRef.current = null;
        if (pendingModelSwitchRef.current === model) {
          pendingModelSwitchRef.current = null;
          modelSwitchConfigCommittedRef.current = false;
          modelSwitchSocketRef.current = null;
          setModelLoading(false);
          setError(t('agent.model_switch_timeout'));
        }
      }, MODEL_SWITCH_TIMEOUT_MS);
    };
    armWatchdog();

    try {
      // Per-agent switch: write THIS agent's own model_provider ref (multi-agent
      // / schema V3). `model` here is a provider ref (e.g. "kilo.minimax_m3").
      // The global `model`/`default_model` keys were removed in V3.
      await putProp(`agents.${agentAlias}.model_provider`, model);
      // The write-phase watchdog may have fired (or a newer switch may have
      // superseded this one) while the request was in flight. Bail before
      // touching the live socket so we never tear it down after giving up.
      if (pendingModelSwitchRef.current !== model) return;
      modelSwitchConfigCommittedRef.current = true;

      // If a turn is actively streaming, abort it on the backend before we tear
      // down the socket. This prevents the old model from continuing to execute
      // tools or persisting its response into the session after we switch.
      if (typingRef.current) {
        const sessionToAbort = activeSessionIdRef.current;
        try {
          await Promise.race([
            abortSession(sessionToAbort),
            new Promise((_, reject) =>
              setTimeout(() => reject(new Error('abort-timeout')), 1_500),
            ),
          ]);
        } catch {
          // Best-effort: if abort fails or times out we still proceed with the
          // switch so the user is never stuck. The old turn may continue on the
          // server, but the UI will show a clean new session.
        }
      }

      // Abort any in-flight streaming before rebuilding the connection.
      foldTurnStream({ type: 'reset' });
      setTyping(false);
      typingRef.current = false;
      // The old socket's request_id no longer maps to anything on the server
      // after we tear it down. Clear here explicitly because we null out the
      // old socket's callbacks below, so its onClose will not fire to do it.
      setPendingApproval(null);

      // Re-arm the watchdog with a fresh budget for the reconnect phase — the
      // one step no awaited promise covers. Bail first if the write phase
      // already timed out (or a newer switch superseded this one).
      if (pendingModelSwitchRef.current !== model) return;
      armWatchdog();

      // Tear down the old socket and create a fresh one.
      // The backend will read the updated config when the new socket opens
      // and construct a new Agent with the selected model.
      const oldWs = wsRef.current;
      if (oldWs) {
        oldWs.onOpen = null;
        oldWs.onClose = null;
        oldWs.onError = null;
        oldWs.onMessage = null;
        oldWs.disconnect();
      }

      const ws = sessionRuntimeRef.current.createSocket({
        agentAlias,
        sessionId: activeSessionIdRef.current,
      });
      // Point wsRef at the NEW client before connect(), so a synchronous
      // connect() throw (e.g. an invalid WebSocket URL/protocol token) still
      // leaves a live, reconnect-capable socket in the ref instead of the old
      // intentionally-closed one — otherwise the page strands offline with no
      // reconnect path until reload.
      wsRef.current = ws as WebSocketClient;
      modelSwitchSocketRef.current = ws;
      attachSocketCallbacks(ws);
      ws.connect();
    } catch (err) {
      // If the per-phase watchdog already fired (timed out) and nulled the
      // pending ref while this request was in flight, bail so a late rejection
      // doesn't overwrite the timeout state. (The `if (modelLoading) return`
      // debounce above prevents truly concurrent switches.)
      if (pendingModelSwitchRef.current !== model) return;
      if (switchTimeoutRef.current) {
        clearTimeout(switchTimeoutRef.current);
        switchTimeoutRef.current = null;
      }
      pendingModelSwitchRef.current = null;
      modelSwitchConfigCommittedRef.current = false;
      modelSwitchSocketRef.current = null;
      setModelLoading(false);
      setError(err instanceof Error ? err.message : t('agent.failed_switch_model'));
    }
  }, [attachSocketCallbacks, modelLoading, typing, agentAlias, foldTurnStream]);

  const deleteMessage = useCallback((id: string) => {
    localMessageMutationVersionRef.current += 1;
    setMessages((prev) => prev.filter((m) => m.id !== id));
  }, []);

  /**
   * Wipe the rendered transcript and any in-flight streaming state. Bumping the
   * mutation version fences off a racing hydration fetch so it can't repopulate
   * the just-cleared view. Shared by "clear chat" and by session switching,
   * which must not show the outgoing conversation while the new one loads.
   */
  const resetTranscriptState = useCallback(() => {
    localMessageMutationVersionRef.current += 1;
    setMessages([]);
    foldTurnStream({ type: 'reset' });
    setTyping(false);
    setPendingApproval(null);
    // Context-window figures describe the transcript we just dropped; leaving
    // them would show the outgoing conversation's token usage against an empty
    // one until the next `done` frame refreshes them.
    setContextInputTokens(null);
    setContextInputSource(null);
    // Same reasoning for the live counters: they describe the session we are
    // leaving. Switching conversations and back must not carry them over.
    setTokenStats(emptyTokenStats);
  }, [foldTurnStream]);

  const clearAllMessages = useCallback(() => {
    resetTranscriptState();

    const sid = activeSessionIdRef.current;
    // Socket identity at the moment of the clear. If it changes while the
    // delete is in flight, a session switch (or another rebuild) has taken over
    // and this closure must not resurrect a connection for the old session.
    const wsAtClear = wsRef.current;

    // The live WebSocket Agent holds the full conversation in memory for the
    // life of the connection, and the gateway persists each turn to the
    // session backend. Clearing only React state leaves both intact, so the
    // next turn still carries prior context and a reload/reconnect repopulates
    // the old transcript (issue #7126). To genuinely reset context we must:
    //   1. delete the persisted backend session history, and
    //   2. tear down + reconnect the socket so the gateway builds a fresh
    //      Agent that seeds from the now-empty backend.
    void (async () => {
      try {
        // Reuses the existing DELETE /api/sessions/{id} primitive
        // (gateway -> SessionBackend::delete_session). Best-effort: a 404 when
        // session persistence is disabled, or any transport failure, must not
        // block the local clear or the reconnect below.
        await sessionRuntimeRef.current.delete(sid);
      } catch {
        // Persistence disabled or request failed — proceed with the reconnect
        // so the live in-memory context is reset regardless.
      }

      if (wsRef.current !== wsAtClear) return;

      // Rebuild the socket so the backend constructs a new Agent with no seeded
      // history. Detach the old socket's callbacks first so its onClose does
      // not write stale connection state. If there is no socket (disconnected),
      // the alias-bound effect will reconnect on its own — nothing to do here.
      const oldWs = wsRef.current;
      if (oldWs) {
        oldWs.onOpen = null;
        oldWs.onClose = null;
        oldWs.onError = null;
        oldWs.onMessage = null;
        oldWs.disconnect();

        const ws = sessionRuntimeRef.current.createSocket({
          agentAlias,
          sessionId: activeSessionIdRef.current,
        });
        // Assign wsRef before connect() so a synchronous throw can't strand the
        // page on the old intentionally-closed socket (see switchModel).
        wsRef.current = ws as WebSocketClient;
        attachSocketCallbacks(ws);
        ws.connect();
      }
    })();
  }, [agentAlias, attachSocketCallbacks, foldTurnStream]);

  const addLocalMessage = useCallback((content: string) => {
    localMessageMutationVersionRef.current += 1;
    setMessages((prev) => [
      ...prev,
      {
        id: generateUUID(),
        role: 'agent',
        content,
        markdown: true,
        timestamp: new Date(),
        ephemeral: true,
      },
    ]);
  }, []);

  /**
   * Point this agent at `nextSessionId`. The socket effect tears down the old
   * connection and reconnects with the new id, and the hydration effect pulls
   * that conversation's transcript — so nothing here touches either directly.
   *
   * A turn streaming in the outgoing conversation is aborted first: the socket
   * is going away regardless, and leaving it running would keep executing tools
   * against a session the operator has navigated away from.
   */
  const transitionToSession = useCallback((nextSessionId: string, force = false): boolean => {
    const currentSessionId = activeSessionIdRef.current;
    if (nextSessionId === currentSessionId) return false;
    // Unknown is deliberately denied alongside false. A failed or in-flight
    // hydration cannot prove there will be a persisted route back. The sole
    // forced caller is a completed delete whose target is now active: that
    // conversation no longer exists, so staying put is not a valid fallback.
    if (!force && sessionPersistenceRef.current !== true) return false;

    if (typingRef.current) {
      void abortSession(currentSessionId).catch(() => {
        // Best-effort: never block the switch on a failed abort.
      });
    }

    resetTranscriptState();
    // Invalidate the hydration marker in the same batch. Without it, switching
    // A -> B -> A before B's fetch lands would leave the marker on A while
    // `messages` is empty, and the mirror effect would write that empty
    // transcript over A's cache — the only copy when persistence is off.
    setHydratedSessionId(null);
    setSessionPersistence(null);
    // Async delete/model operations consult these refs before React commits
    // the state update. Move them with the transition so two completions in
    // the same task cannot both act on the conversation we just left.
    activeSessionIdRef.current = nextSessionId;
    sessionPersistenceRef.current = null;
    typingRef.current = false;
    // The pointer records the alias's most recently selected conversation, so a
    // deep link or a fresh workspace resumes there. With the same agent open in
    // two panes the last switch wins, which is why a pane that has an owner
    // also reports upward — the owner, not the pointer, is what restores each
    // pane to its own conversation.
    setActiveSessionId(agentAlias, nextSessionId);
    setSessionId(nextSessionId);
    onSessionChange?.(nextSessionId);
    return true;
  }, [agentAlias, onSessionChange, resetTranscriptState]);

  const goToSession = useCallback((nextSessionId: string): boolean => (
    transitionToSession(nextSessionId)
  ), [transitionToSession]);

  /** Start an additional conversation, leaving the current one intact. */
  const startNewSession = useCallback((): boolean => {
    // Do not even mint an unreachable id until the shared transition gate is
    // open. goToSession rechecks the capability before mutating state.
    if (sessionPersistenceRef.current !== true) return false;
    return transitionToSession(sessionRuntimeRef.current.mintId());
  }, [transitionToSession]);

  /**
   * Drop a stored conversation. Deleting the one on screen leaves nothing to
   * display, so the agent moves to a fresh conversation rather than showing a
   * transcript the gateway no longer has.
   */
  const removeSession = useCallback(async (targetSessionId: string) => {
    if (sessionPersistenceRef.current !== true) {
      throw new Error('Session persistence is not available');
    }
    try {
      await sessionRuntimeRef.current.delete(targetSessionId);
    } catch (err) {
      // 404 means the gateway has nothing stored for this conversation, which
      // is already the outcome the operator asked for: a row another client
      // deleted, or one the picker still shows as "Not saved yet". The session
      // endpoints answer with a plain `{"error": ...}` body rather than the
      // structured envelope, so read the status from either error shape.
      // Anything else is a real failure — the transcript is still on the
      // server, so say so instead of quietly dropping the row.
      const status = err instanceof ApiError || err instanceof HttpError ? err.status : undefined;
      if (status !== 404) throw err;
    }
    // The browser cache is a second copy of the same transcript; a delete that
    // leaves it behind is not a delete.
    removeChatHistory(targetSessionId);
    // Decide at completion, not at invocation. The target may have become
    // active while the request was in flight (B delete -> select B), or may
    // have become active again (A -> B -> A). In either case the deleted
    // conversation must be replaced even though its hydration capability is
    // currently unknown. Conversely, A -> B must leave B alone.
    if (targetSessionId === activeSessionIdRef.current) {
      transitionToSession(sessionRuntimeRef.current.mintId(), true);
    }
  }, [transitionToSession]);

  const renameConversation = useCallback(async (targetSessionId: string, name: string) => {
    if (sessionPersistence !== true) {
      throw new Error('Session persistence is not available');
    }
    await sessionRuntime.rename(targetSessionId, name);
  }, [sessionPersistence, sessionRuntime]);

  const respondToApproval = useCallback((decision: ApprovalDecision) => {
    setPendingApproval((current) => {
      if (!current) return null;
      try {
        wsRef.current?.sendApprovalResponse(current.requestId, decision);
      } catch {
        // Socket closed mid-prompt; backend will auto-deny on its side.
      }
      return null;
    });
  }, []);

  const value: AgentContextValue = {
    messages,
    sendMessage,
    connected,
    error,
    typing,
    currentModel,
    availableModels,
    switchModel,
    modelLoading,
    refreshModels: () => setModelInfoVersion((v) => v + 1),
    deleteMessage,
    clearAllMessages,
    sessionId,
    hydrated: hydratedSessionId === sessionId,
    sessionPersistence,
    reservedSessionIds: reservedSessionIds ?? NO_RESERVED_SESSIONS,
    startNewSession,
    goToSession,
    removeSession,
    renameConversation,
    addLocalMessage,
    abortSession: async () => {
      // Clear local approval state immediately — the in-flight request_id
      // belongs to the turn we're cancelling and will be rejected by the
      // backend on a late click anyway. Don't wait for the `aborted` frame
      // to round-trip; the user clicked Stop and expects the UI to follow.
      setPendingApproval(null);
      try {
        await abortSession(sessionId);
      } catch {
        // Best-effort abort
      }
    },
    pendingApproval,
    respondToApproval,
    // Context window tracking (from "done" WS frames). See #7311.
    contextEffectiveWindow,
    contextTrimBudgetTokens,
    contextInputTokens,
    contextInputSource,
    liveStats,
    liveTurn,
  };

  return <AgentContext.Provider value={value}>{children}</AgentContext.Provider>;
}
