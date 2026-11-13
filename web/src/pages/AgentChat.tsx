import { memo, useState, useEffect, useLayoutEffect, useMemo, useRef, useCallback } from 'react';
import { Link, Navigate, useParams } from 'react-router-dom';
import { Bot, User, AlertCircle, Copy, Check, X, Trash2, Minimize2, Maximize2, ChevronDown, Wrench, Brain, FolderOpen, MoreVertical } from 'lucide-react';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { useAgent, type ChatMessage } from '@/contexts/AgentContext';
import { ApiError, uploadChatImage } from '@/lib/api';
import { useDraft } from '@/hooks/useDraft';
import { useAvailableAgents, useNavAccess } from '@/hooks/useNavAccess';
import { t } from '@/lib/i18n';
import {
  commandsFor,
  helpText,
  isSlashCommand,
  matchCommands,
  parseCommand,
  type CommandSpec,
} from '@/lib/slashCommands';
import { Button, Card } from '@/components/ui';
import { NotAvailable } from '@/components/auth/NotAvailable';
import { stripServerTimestamp } from '@/lib/stripServerTimestamp';
import {
  createFollowState,
  foldScroll,
  resumeFollowing,
  shouldRepinOnGroupToggle,
  shouldRepinOnResize,
  type FollowState,
} from '@/pages/agentChatScroll.logic';
import ChatWorkspace from '@/pages/ChatWorkspace';

import ToolCallCard from '@/components/ToolCallCard';
import ToolCallGroup from '@/components/ToolCallGroup';
import ChatImage from '@/components/ChatImage';
import { hasChatImage, stepsHaveChatImage } from '@/components/chatImage.logic';
import ApprovalBanner from '@/components/ApprovalBanner';
import SessionPicker from '@/components/SessionPicker';
import InputBar from '@/components/InputBar';
import {
  groupMessages,
  pendingOutputIds,
  splitLiveTurn,
  type RenderBlock,
} from '@/lib/messageFlow.logic';
import type { TurnSegments } from '@/lib/turnSegments';

const DRAFT_KEY_PREFIX = 'agent-chat';

// Open chat links in a new tab so navigation never replaces the live chat
// page. In-page anchors (e.g. GFM footnote refs) keep default navigation.
const markdownComponents: Components = {
  a: ({ node: _node, href, ...props }) =>
    href?.startsWith('#') ? (
      <a {...props} href={href} />
    ) : (
      <a {...props} href={href} target="_blank" rel="noopener noreferrer" />
    ),
  img: ChatImage,
};

/**
 * Route entry point for `/agent/:alias`. Reads the alias from the URL and
 * hands it to the multi-agent ChatWorkspace as the initial chat to open and
 * activate. The workspace itself owns the set of open chats and never
 * remounts on tab/layout switches, so the alias is passed as a prop (not used
 * as a React `key`) — that keeps every chat's AgentProvider WebSocket alive
 * across tab switches. Missing alias → redirect to the agents list.
 */
export default function AgentChat() {
  const { alias } = useParams<{ alias: string }>();
  const access = useNavAccess();
  if (!alias) {
    return <Navigate to="/agents" replace />;
  }
  if (!access.operatorPages) {
    return <ScopedChatWorkspace alias={alias} />;
  }
  return <ChatWorkspace initialAlias={alias} />;
}

/**
 * A signed-in account without the operator dashboard chats only with the
 * agents `/api/agents/available` lists. The gateway refuses any other agent
 * before the socket opens, which the browser would otherwise retry forever,
 * so an unlisted alias shows "not available" and stale tabs for other agents
 * are dropped. A list that failed to load says nothing about the account's
 * access, so it offers another try instead.
 */
function ScopedChatWorkspace({ alias }: { alias: string }) {
  const { loading, agents, error, retry } = useAvailableAgents(true);
  const allowed = useMemo(
    () => new Set(agents.filter((a) => a.enabled).map((a) => a.alias)),
    [agents],
  );
  if (loading) {
    return (
      <div className="min-h-[60vh] flex items-center justify-center">
        <div className="h-8 w-8 border-2 rounded-full animate-spin border-pc-border border-t-pc-accent" />
      </div>
    );
  }
  if (error !== null) {
    return <AvailabilityLoadError error={error} onRetry={retry} />;
  }
  if (!allowed.has(alias)) {
    return <NotAvailable access="not-available" />;
  }
  return <ChatWorkspace initialAlias={alias} allowedAliases={allowed} />;
}

/** The agents list failed to load: the error and a Retry, not a refusal. */
function AvailabilityLoadError({ error, onRetry }: { error: string; onRetry: () => void }) {
  return (
    <div className="p-6">
      <Card padded={false} className="p-6 w-full max-w-lg" data-agents-load-error="">
        <div className="flex items-center gap-2 mb-2">
          <AlertCircle className="h-5 w-5 shrink-0 text-status-error" />
          <h2 className="text-lg font-semibold" style={{ color: 'var(--pc-text-primary)' }}>
            {t('agent.availability_load_failed')}
          </h2>
        </div>
        {error && (
          <p className="text-sm mb-4 break-words" style={{ color: 'var(--pc-text-muted)' }}>
            {error}
          </p>
        )}
        <Button size="sm" onClick={onRetry} data-agents-retry="">
          {t('common.retry')}
        </Button>
      </Card>
    </div>
  );
}

/** Status snapshot a chat pane pushes up to the workspace tab bar. */
export interface AgentChatStatus {
  typing: boolean;
  messageCount: number;
}

/**
 * Full chat view for a single agent. Must be rendered inside an
 * `<AgentProvider>` (it calls `useAgent()` internally). Exported so the
 * multi-agent `ChatWorkspace` can mount one instance per open chat and keep
 * them all alive simultaneously.
 *
 * `onStatus` lets the host (the workspace) observe live typing / message-count
 * changes per pane without itself subscribing to the agent context — used to
 * drive the streaming and unread indicators in the tab bar.
 */
export function AgentChatInner({
  agentAlias,
  onStatus,
}: {
  agentAlias: string;
  onStatus?: (s: AgentChatStatus) => void;
}) {
  const {
    messages,
    sendMessage,
    connected,
    conversationLimited,
    retryConnection,
    conversationPerSocket,
    startsConversationOnSend,
    unsentMessage,
    takeUnsentMessage,
    error,
    typing,
    currentModel,
    availableModels,
    switchModel,
    modelLoading,
    deleteMessage,
    clearAllMessages,
    startNewSession,
    sessionId,
    hydrated,
    addLocalMessage,
    abortSession,
    pendingApproval,
    respondToApproval,
    contextEffectiveWindow,
    contextTrimBudgetTokens,
    contextInputTokens,
    contextInputSource,
    liveStats,
    liveTurn,
    hasEarlierTurns,
    loadingEarlier,
    loadEarlierTurns,
    loadToolOutputs,
  } = useAgent();
  // A signed-in account without the operator dashboard can't switch the
  // agent's model (that rewrites its config for everyone), upload, or open the
  // agent's workspace.
  const operator = useNavAccess().operatorPages;
  const commands = useMemo(() => commandsFor(operator), [operator]);
  // With one conversation per socket, no socket is open between
  // conversations, and a message is what opens the next one.
  const canSend = connected || startsConversationOnSend;

  // Keyed by conversation, not just alias: with the same agent open in two
  // panes an alias-only key would make both share one draft, so typing in one
  // would overwrite the other's unsent message.
  const draftKey = `${DRAFT_KEY_PREFIX}.${agentAlias}.${sessionId}`;
  const { draft, saveDraft, clearDraft } = useDraft(draftKey);
  const [input, setInput] = useState(draft);
  // Mirror of `input` for writers that run after an await (an attachment
  // marker landing once the operator has typed more). A functional update
  // against React state alone could not also hand the final value to the draft
  // store, so the updater form of applyInput resolves against this instead.
  const inputValueRef = useRef(input);
  const writeInput = useCallback((value: string) => {
    inputValueRef.current = value;
    setInput(value);
  }, []);

  // `saveDraft` is bound to the conversation on screen when it was created.
  // A deferred writer (an upload that lands after the operator switched
  // conversations) reads the current text from `inputValueRef`, so it must
  // also write under the current key; using the key it captured would park
  // the text in the old conversation while showing it in the new one.
  // Resolve both at invocation time. Whether a deferred writer should instead
  // be dropped after a switch is the caller's decision.
  const saveDraftRef = useRef(saveDraft);
  saveDraftRef.current = saveDraft;

  /**
   * Single writer for the live composer text. Every change to what the
   * operator has typed (keystrokes, slash-command completion, a marker
   * appended by a later feature) goes through here so the per-conversation
   * draft store stays in step with the textarea. A bare `setInput` would leave
   * the stored draft behind and lose the change on the next conversation
   * switch. Accepts a value or an updater over the latest text; both the text
   * and the draft key are the ones current when it runs, not when it was
   * captured.
   */
  const applyInput = useCallback((next: string | ((prev: string) => string)) => {
    const value = typeof next === 'function' ? next(inputValueRef.current) : next;
    writeInput(value);
    saveDraftRef.current(value);
  }, [writeInput]);
  const [showModelDropdown, setShowModelDropdown] = useState(false);
  const [showMoreMenu, setShowMoreMenu] = useState(false);
  // Slash-command autocomplete popover (#7137). Shown while the input begins
  // with a single '/' and the token still matches at least one command.
  const [showCommandHint, setShowCommandHint] = useState(false);
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const [compact, setCompact] = useState(() => {
    try { return localStorage.getItem('zeroclaw_chat_compact') === '1'; } catch { return false; }
  });
  // Tool execution is plumbing, not chat. Default off so tool_call /
  // tool_result frames do not surface inline in the conversation transcript.
  // Toggleable from the chat toolbar (Wrench button). The WebSocket lives in
  // AgentContext, which always pushes tool cards into messages; this toggle
  // filters them at render time so toggling on retroactively reveals prior
  // tool activity.
  const [showToolActivity, setShowToolActivity] = useState(() => {
    try { return localStorage.getItem('zeroclaw_show_tool_activity') === '1'; } catch { return false; }
  });
  // When set, the live Thinking box starts collapsed and is left to the user
  // to open; by default it is open until the answer starts.
  const [keepThinkingCollapsed, setKeepThinkingCollapsed] = useState(() => {
    try { return localStorage.getItem('zeroclaw_keep_thinking_collapsed') === '1'; } catch { return false; }
  });

  /**
   * Move the pane to the tail, deferring to the next frame.
   *
   * A state update that just grew the content (an expanded group, a toggled
   * `<details>`) has not been laid out yet when this runs, so reading
   * `scrollHeight` synchronously would re-pin against the previous layout and
   * land short of the bottom. `requestAnimationFrame` runs after layout, so the
   * height it reads is the one the reader will see.
   *
   * Deliberately not gated on `followRef` here: callers decide whether
   * re-pinning is warranted (see `shouldRepinOnGroupToggle`).
   */
  const pinToTail = useCallback(() => {
    requestAnimationFrame(() => {
      const el = scrollerRef.current;
      if (!el) return;
      el.scrollTop = el.scrollHeight;
      followRef.current = {
        ...followRef.current,
        lastScrollTop: el.scrollTop,
        lastScrollHeight: el.scrollHeight,
        lastClientHeight: el.clientHeight,
      };
    });
  }, []);

  // Which turns' tool-call groups the reader has expanded. A finished turn is
  // collapsed by default (matching the reference); the reader's toggle wins and
  // is remembered per turn. Keyed by message id, so it survives re-renders and
  // dies with the transcript.
  const [expandedGroups, setExpandedGroups] = useState<Record<string, boolean>>({});
  const toggleGroup = useCallback((messageId: string) => {
    setExpandedGroups((prev) => {
      const next = !prev[messageId];
      // Expanding appends below the reader's offset, so a pane that was at the
      // tail stays there — and the turn they just expanded is the one they
      // wanted to read. Collapsing would drag the offset down past text they
      // were reading, so it never moves the pane.
      if (shouldRepinOnGroupToggle(!next, followRef.current.following)) pinToTail();
      return { ...prev, [messageId]: next };
    });
  }, [pinToTail]);

  // Toggle a finished turn's group; opening it fetches the tool output its
  // hydrated cards only reference.
  const openGroup = useCallback((groupId: string, segments: TurnSegments) => {
    const opening = !expandedGroups[groupId];
    toggleGroup(groupId);
    if (!opening) return;
    const ids = pendingOutputIds(segments);
    if (ids.length > 0) void loadToolOutputs(ids);
  }, [expandedGroups, toggleGroup, loadToolOutputs]);

  // The in-flight turn's group is expanded while it streams and collapses once
  // the committed message takes over (D1). That is also why no toggle carries
  // over: the only toggle the reader can make on a live group is "collapse",
  // which is what the committed group defaults to anyway.
  const [liveGroupCollapsed, setLiveGroupCollapsed] = useState(false);
  const toggleLiveGroup = useCallback(() => {
    setLiveGroupCollapsed((prev) => {
      // Same rule as the committed group: expanding appends below the reader
      // and may re-pin; collapsing never does.
      if (shouldRepinOnGroupToggle(!prev, followRef.current.following)) pinToTail();
      return !prev;
    });
  }, [pinToTail]);
  useEffect(() => {
    // A fresh turn starts expanded. `typing` stays true for the whole turn, so
    // this fires on the boundary rather than on every frame.
    if (typing) setLiveGroupCollapsed(false);
  }, [typing]);

  // Split the in-flight turn the way the committed one is split. Pure and
  // unit-tested in `lib/messageFlow.logic`; the rules it encodes (which step
  // is still the answer, and why a tool-free turn never grows a group) live
  // there rather than here.
  const live = useMemo(() => splitLiveTurn(liveTurn), [liveTurn]);

  // Fold the flat message list into render blocks: a live turn's loose tool
  // cards collapse into one turn block carrying its step trajectory. Hydrated
  // turns have no trajectory and stay plain. Pure and unit-tested in
  // `lib/messageFlow.logic`.
  const blocks: RenderBlock[] = useMemo(
    () => groupMessages(messages, { showToolActivity, liveSteps: live.groupSteps }),
    [messages, showToolActivity, live.groupSteps],
  );

  // Transcript pane + reader-intent state. The follow decision lives in a ref
  // (written by the scroll listener below) so a streamed update never needs a
  // re-render to find out whether it may move the pane.
  const scrollerRef = useRef<HTMLDivElement>(null);
  // The transcript itself, not the scrollport. The port's box stays the
  // viewport height; this node's box is what grows when an image decodes.
  const contentRef = useRef<HTMLDivElement>(null);
  const followRef = useRef<FollowState>(createFollowState());
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const modelDropdownRef = useRef<HTMLDivElement>(null);
  const moreMenuRef = useRef<HTMLDivElement>(null);

  // AgentChatInner stays mounted while the selected conversation changes.
  // Load that conversation's draft explicitly; useState's initializer only ran
  // for the first session and cannot provide this synchronization by itself.
  useEffect(() => {
    writeInput(draft);
  }, [draftKey, draft, writeInput]);

  // A message that never reached a conversation comes back to the composer,
  // ahead of anything typed since.
  useEffect(() => {
    if (unsentMessage === null) return;
    applyInput((typed) => (typed ? `${unsentMessage}\n${typed}` : unsentMessage));
    takeUnsentMessage();
  }, [unsentMessage, applyInput, takeUnsentMessage]);

  // Report live status (typing + message count) up to the host workspace so it
  // can render streaming / unread indicators in the tab bar. Fires on every
  // typing flip or message-count change; the workspace decides what to do with
  // it (e.g. mark a hidden tab unread when its count grows).
  useEffect(() => {
    onStatus?.({ typing, messageCount: messages.length });
  }, [typing, messages.length, onStatus]);

  // A conversation switch always lands on the newest message, even when the
  // reader had scrolled up in the previous one. Declared before the follow
  // effect below so both run in the same commit, reset first. Seeded with the
  // offset still on screen: the pane is reused across conversations, so a
  // hardcoded 0 would swallow the reader's first backwards move.
  useEffect(() => {
    followRef.current = createFollowState(scrollerRef.current?.scrollTop ?? 0);
  }, [sessionId]);

  // Sample reader intent from the pane's own scroll events.
  useEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const onScroll = () => {
      followRef.current = foldScroll(followRef.current, {
        scrollTop: el.scrollTop,
        scrollHeight: el.scrollHeight,
        clientHeight: el.clientHeight,
      });
    };
    el.addEventListener('scroll', onScroll, { passive: true });
    return () => el.removeEventListener('scroll', onScroll);
  }, []);

  // Keep the newest output in view, but only while the reader is at the tail
  // (#9562). Two details matter:
  //   - the write is instant, never `behavior: 'smooth'`. Streamed updates
  //     arrive far faster than a smooth animation completes, so each one
  //     restarted the animation and the pane never settled at the bottom;
  //   - a suspended pane does nothing at all, so the reader can finish the
  //     message they scrolled back to before following resumes.
  // Note: WebSocket lifecycle, hydration, and tool_call/tool_result handling
  // moved to AgentContext (PR #6101). Tool activity is filtered at render
  // time below using `showToolActivity`, not at the message-handler layer.
  useEffect(() => {
    const el = scrollerRef.current;
    if (!el || !followRef.current.following) return;
    el.scrollTop = el.scrollHeight;
  }, [messages, typing, liveTurn]);

  // Prepending earlier turns grows the transcript above the reader. Record the
  // scroll offset and height before the load, then shift by the growth once
  // the new turns are laid out, so the turn at the top stays put.
  const prependAnchorRef = useRef<{ top: number; height: number } | null>(null);
  const handleLoadEarlier = useCallback(async () => {
    const el = scrollerRef.current;
    if (el) prependAnchorRef.current = { top: el.scrollTop, height: el.scrollHeight };
    await loadEarlierTurns();
    // A load that prepended nothing must not shift a later update.
    requestAnimationFrame(() => {
      prependAnchorRef.current = null;
    });
  }, [loadEarlierTurns]);
  useLayoutEffect(() => {
    const anchor = prependAnchorRef.current;
    const el = scrollerRef.current;
    if (!anchor || !el) return;
    prependAnchorRef.current = null;
    el.scrollTop = anchor.top + (el.scrollHeight - anchor.height);
  }, [messages]);

  // Re-pin when the transcript grows on its own, without an update of its own
  // to react to. The effect above only runs when `messages`/`typing`/`liveTurn`
  // change, so it writes the offset for the layout as it was at that moment —
  // and a markdown image has no height until the browser decodes it, which
  // happens later and fires no scroll event. The result was a pane stuck short
  // of the bottom whose newest output could only be reached by scrolling by
  // hand.
  //
  // Observe the content wrapper, not the scrollport and not its first child.
  // The port's own box does not change when its content does, and the first
  // child is only the first message — an image in a later message never
  // resizes it. A shrink (collapsed group, image settling under max-height)
  // must not re-pin; that would jump the reader past text they were reading.
  useEffect(() => {
    const content = contentRef.current;
    if (!content || typeof ResizeObserver === 'undefined') return;

    let lastHeight = content.getBoundingClientRect().height;
    const observer = new ResizeObserver(() => {
      const next = content.getBoundingClientRect().height;
      const grew = next > lastHeight;
      lastHeight = next;
      if (!shouldRepinOnResize(followRef.current.following, grew)) return;
      pinToTail();
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, [pinToTail]);

  // Re-pin when the scrollport itself gets shorter: the on-screen keyboard
  // opening, or the composer growing a line. The offset stays where it was, so
  // the tail slides out under the composer and no scroll event says so. A
  // taller port needs nothing: the browser clamps the offset onto the tail and
  // `foldScroll` reads that clamp as the pane's own doing.
  useEffect(() => {
    const el = scrollerRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;

    let lastHeight = el.clientHeight;
    const observer = new ResizeObserver(() => {
      const next = el.clientHeight;
      const shrank = next < lastHeight;
      lastHeight = next;
      if (followRef.current.following && shrank) pinToTail();
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [pinToTail]);

  // Close model / more dropdowns when clicking outside
  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      const target = e.target as Node;
      if (modelDropdownRef.current && !modelDropdownRef.current.contains(target)) {
        setShowModelDropdown(false);
      }
      if (moreMenuRef.current && !moreMenuRef.current.contains(target)) {
        setShowMoreMenu(false);
      }
    }
    document.addEventListener('mousedown', handleClickOutside);
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, []);

  /**
   * Slash-command dispatcher (#7137). Returns true when `trimmed` was handled
   * as a command (and therefore must NOT be sent to the model as a prompt).
   * Commands drive existing session primitives — clear/reset and model switch —
   * and surface their feedback as local info messages, never to the gateway.
   */
  const runCommand = useCallback((trimmed: string): boolean => {
    if (!isSlashCommand(trimmed)) return false;

    const { command, args } = parseCommand(trimmed);
    switch (command) {
      case 'help':
        addLocalMessage(helpText(commands));
        return true;

      case 'clear':
        clearAllMessages();
        addLocalMessage(t('agent.cmd_cleared'));
        return true;

      // Was an alias for /clear, which deleted the conversation. Now that an
      // agent can hold several, "new" means what it says: start another one and
      // leave this one on the gateway (issue #7543).
      case 'new':
        if (!startNewSession()) {
          addLocalMessage(t('agent.sessions_unavailable'));
        }
        return true;

      case 'upload':
        if (!operator) {
          addLocalMessage(t('auth.not_available'));
          return true;
        }
        // Same picker as the attach button — the input click is allowed here
        // because runCommand fires from the user's Enter keypress, keeping
        // the browser's user-activation requirement satisfied.
        fileInputRef.current?.click();
        return true;

      case 'model': {
        if (!operator) {
          addLocalMessage(t('auth.not_available'));
          return true;
        }
        const name = args.trim();
        if (!name) {
          const current = currentModel
            ? t('agent.cmd_model_current').replace('{model}', currentModel)
            : t('agent.cmd_model_none');
          const list = availableModels.length > 0
            ? `\n${t('agent.cmd_model_available').replace('{models}', availableModels.join(', '))}`
            : '';
          addLocalMessage(`${current}${list}`);
          return true;
        }
        if (name === currentModel) {
          addLocalMessage(t('agent.cmd_model_current').replace('{model}', name));
          return true;
        }
        if (availableModels.length > 0 && !availableModels.includes(name)) {
          addLocalMessage(
            t('agent.cmd_model_unknown')
              .replace('{model}', name)
              .replace('{models}', availableModels.join(', ')),
          );
          return true;
        }
        // switchModel silently no-ops while another switch is in flight, which
        // looks like the command was ignored. Surface that state explicitly. #7137
        if (modelLoading) {
          addLocalMessage(t('agent.cmd_model_busy'));
          return true;
        }
        addLocalMessage(t('agent.cmd_model_switching').replace('{model}', name));
        // Reuse the existing model-switch path (config write + socket rebuild).
        void switchModel(name).catch(() => {
          // switchModel surfaces its own error via context `error` state, but
          // the user just typed a command and expects inline feedback there too.
          addLocalMessage(t('agent.cmd_model_failed').replace('{model}', name));
        });
        return true;
      }

      default:
        addLocalMessage(t('agent.cmd_unknown').replace('{cmd}', `/${command}`));
        return true;
    }
  }, [addLocalMessage, clearAllMessages, startNewSession, currentModel, availableModels, switchModel, modelLoading, operator, commands]);

  const handleSend = () => {
    const trimmed = input.trim();
    if (!trimmed) return;

    // Submitting from the composer is an explicit "show me the tail": the input
    // sits below the transcript, so the reader expects their own message and
    // the reply to land there even if they had scrolled up to read history.
    // Every other streamed update leaves the pane where the reader left it
    // (#9562).
    followRef.current = resumeFollowing(followRef.current);

    // Slash commands are dispatched BEFORE the connectivity check so purely
    // local commands like /help still work during transient disconnects.
    // Network-dependent commands (/clear, /model) self-recover via their own
    // reconnect paths inside the context. #7137
    if (isSlashCommand(trimmed)) {
      runCommand(trimmed);
    } else {
      if (!canSend) return;
      // `//` is the documented escape for a literal leading slash (#7223);
      // strip one `/` so `//foo` is sent to the agent as `/foo`.
      sendMessage(trimmed.startsWith('//') ? trimmed.slice(1) : trimmed);
    }
    setShowCommandHint(false);
    writeInput('');
    clearDraft();
    if (inputRef.current) {
      inputRef.current.style.height = 'auto';
      inputRef.current.focus();
    }
  };

  const isComposingRef = useRef(false);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Escape' && showCommandHint) {
      setShowCommandHint(false);
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing && !isComposingRef.current) {
      e.preventDefault();
      handleSend();
    }
  };

  // ── Image upload (picker + drag-and-drop) ──────────────────────────────
  // Both gestures funnel into one helper: upload each file, then append the
  // returned [IMAGE:<path>] marker to the draft so the user can add context
  // before sending. The gateway sniffs the payload by magic bytes; the
  // client-side type filter only spares an obviously wrong file the round
  // trip. Failures surface as chat-local messages (same channel as unknown
  // slash commands), never as sent messages.
  const [uploading, setUploading] = useState(false);
  const [dragActive, setDragActive] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  // dragenter/dragleave fire per child element; a depth counter keeps the
  // overlay stable while the pointer crosses inner boundaries.
  const dragDepthRef = useRef(0);

  const uploadImages = useCallback(async (files: Iterable<File>) => {
    const list = Array.from(files);
    if (list.length === 0) return;
    setUploading(true);
    try {
      for (const file of list) {
        if (file.type && !file.type.startsWith('image/')) {
          addLocalMessage(t('agent.upload_not_image').replace('{name}', file.name));
          continue;
        }
        try {
          const res = await uploadChatImage(agentAlias, file);
          applyInput((prev) => `${prev && !prev.endsWith(' ') ? `${prev} ` : prev}${res.marker} `);
        } catch (err) {
          const message = err instanceof ApiError
            ? err.envelope.message
            : err instanceof Error ? err.message : String(err);
          addLocalMessage(t('agent.upload_failed').replace('{error}', message));
        }
      }
    } finally {
      setUploading(false);
      inputRef.current?.focus();
    }
  }, [agentAlias, addLocalMessage, applyInput]);

  const dragHasFiles = (e: React.DragEvent) => e.dataTransfer.types.includes('Files');

  const handleDragEnter = (e: React.DragEvent) => {
    if (!dragHasFiles(e)) return;
    e.preventDefault();
    if (!operator) return;
    dragDepthRef.current += 1;
    setDragActive(true);
  };
  const handleDragOver = (e: React.DragEvent) => {
    if (dragHasFiles(e)) e.preventDefault();
  };
  const handleDragLeave = (e: React.DragEvent) => {
    if (!dragHasFiles(e) || !operator) return;
    dragDepthRef.current = Math.max(0, dragDepthRef.current - 1);
    if (dragDepthRef.current === 0) setDragActive(false);
  };
  const handleDrop = (e: React.DragEvent) => {
    if (!dragHasFiles(e)) return;
    // Without upload the drop is still swallowed, so the browser doesn't
    // navigate away to the file.
    e.preventDefault();
    dragDepthRef.current = 0;
    setDragActive(false);
    if (!operator) return;
    void uploadImages(e.dataTransfer.files);
  };

  const handleTextareaChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const value = e.target.value;
    applyInput(value);
    // Show the command popover while typing the command token (a single
    // leading '/' with no space yet). Hide once the user moves to arguments or
    // the token no longer matches any command.
    const showHint = /^\/[^/\s]*$/.test(value) && matchCommands(value.slice(1), commands).length > 0;
    setShowCommandHint(showHint);
    e.target.style.height = 'auto';
    e.target.style.height = `${Math.min(e.target.scrollHeight, 200)}px`;
  };

  // Apply a command from the autocomplete popover: fill the input with the
  // command and a trailing space (for arg-taking commands) or dispatch it
  // immediately when it takes no further input.
  const applyCommandHint = useCallback((spec: CommandSpec) => {
    setShowCommandHint(false);
    const takesArgs = spec.usage.includes('[');
    const value = `/${spec.name}${takesArgs ? ' ' : ''}`;
    applyInput(value);
    inputRef.current?.focus();
  }, [applyInput]);

  const matchedCommands: CommandSpec[] = /^\/[^/\s]*$/.test(input)
    ? matchCommands(input.slice(1), commands)
    : commands.slice();

  const handleCopy = useCallback((msgId: string, content: string) => {
    const onSuccess = () => {
      setCopiedId(msgId);
      setTimeout(() => setCopiedId((prev) => (prev === msgId ? null : prev)), 2000);
    };

    if (navigator.clipboard?.writeText) {
      navigator.clipboard.writeText(content).then(onSuccess).catch(() => {
        fallbackCopy(content) && onSuccess();
      });
    } else {
      fallbackCopy(content) && onSuccess();
    }
  }, []);

  const handleDeleteMessage = useCallback((msgId: string) => {
    deleteMessage(msgId);
  }, [deleteMessage]);

  const handleClearAll = useCallback(() => {
    clearAllMessages();
  }, [clearAllMessages]);

  // Stop button: POST /api/sessions/{id}/abort. The gateway cancels the
  // in-flight turn, the WS handler sends an `error` frame which our
  // onMessage handler already maps to typing=false.
  const handleAbort = useCallback(async () => {
    try {
      await abortSession();
    } catch {
      // Best-effort: surface nothing if the abort itself fails. The
      // user can retry, and any leaked typing state clears on the next
      // server frame.
    }
  }, [abortSession]);

  const toggleCompact = useCallback(() => {
    setCompact((prev) => {
      const next = !prev;
      try { localStorage.setItem('zeroclaw_chat_compact', next ? '1' : '0'); } catch { /* noop */ }
      return next;
    });
  }, []);

  const toggleToolActivity = useCallback(() => {
    setShowToolActivity((prev) => {
      const next = !prev;
      try { localStorage.setItem('zeroclaw_show_tool_activity', next ? '1' : '0'); } catch { /* noop */ }
      return next;
    });
  }, []);

  const toggleKeepThinkingCollapsed = useCallback(() => {
    setKeepThinkingCollapsed((prev) => {
      const next = !prev;
      try { localStorage.setItem('zeroclaw_keep_thinking_collapsed', next ? '1' : '0'); } catch { /* noop */ }
      return next;
    });
  }, []);

  /**
   * Fallback copy using a temporary textarea for HTTP contexts
   * where navigator.clipboard is unavailable.
   */
  function fallbackCopy(text: string): boolean {
    const textarea = document.createElement('textarea');
    textarea.value = text;
    textarea.style.position = 'fixed';
    textarea.style.opacity = '0';
    document.body.appendChild(textarea);
    textarea.select();
    try {
      document.execCommand('copy');
      return true;
    } catch {
      return false;
    } finally {
      document.body.removeChild(textarea);
    }
  }

  const handleModelSwitch = async (model: string) => {
    setShowModelDropdown(false);
    if (model === currentModel) return;
    try {
      await switchModel(model);
    } catch {
      // Error is already set by switchModel internally
    }
  };

  return (
    /* translate="no" / notranslate (#7057): browser auto-translation (e.g.
       Chrome → Google Translate) rewrites text nodes into <font> wrappers.
       React reconciliation then trips "Failed to execute 'removeChild' on
       'Node'" and unmounts the view. The crash repro surface spans every
       dynamic-text region on this page: streaming output, ReactMarkdown
       message bodies, the {error} banner above the toolbar, and
       ApprovalBanner (whose <pre>{argumentsSummary}</pre> and per-second
       remainingSec re-render are at least as crash-prone as streaming).
       Hoisting the opt-out to the outermost container covers all of them
       with a single ancestor. Static UI chrome here localizes through
       t() i18n, so losing browser translation on it is intentional. */
    <div
      translate="no"
      className="notranslate relative flex flex-col h-full min-h-0"
      onDragEnter={handleDragEnter}
      onDragOver={handleDragOver}
      onDragLeave={handleDragLeave}
      onDrop={handleDrop}
    >
      {dragActive && (
        <div
          className="absolute inset-2 z-50 flex items-center justify-center pointer-events-none rounded-[var(--radius-md)]"
          style={{
            border: '2px dashed var(--pc-accent)',
            background: 'color-mix(in srgb, var(--pc-accent) 10%, transparent)',
          }}
        >
          <span
            className="px-3 py-1.5 text-sm font-medium rounded-[var(--radius-md)]"
            style={{ background: 'var(--pc-bg-surface)', color: 'var(--pc-accent)' }}
          >
            {t('agent.drop_to_attach')}
          </span>
        </div>
      )}
      {/* Header: agent + Files + chat actions + session/model (one row).
          Actions always visible (previously only when messages.length > 0,
          which made the merge look like a no-op on empty chats). */}
      <div className="relative flex min-w-0 items-center justify-between gap-2 px-3 sm:px-4 py-2 border-b border-pc-border bg-pc-surface">
        <div className="flex min-w-0 shrink-0 sm:shrink items-center gap-1.5 sm:gap-2">
          <Bot className="h-4 w-4 shrink-0 text-pc-accent" />
          <span className="min-w-0 max-w-[5.5rem] sm:max-w-[12rem] truncate text-sm font-medium text-pc-text" title={agentAlias}>
            {agentAlias}
          </span>
          {operator && (
            <Link
              to={`/agent/${encodeURIComponent(agentAlias)}/workspace`}
              className="inline-flex shrink-0 items-center gap-1 px-1.5 sm:px-2 h-6 rounded-[var(--radius-md)] text-xs font-medium text-pc-text-secondary transition-colors hover:text-pc-text hover:bg-[var(--pc-hover)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)]"
              title={t('agentchat.open_workspace')}
              aria-label={t('agentchat.open_workspace')}
            >
              <FolderOpen className="h-3.5 w-3.5" />
              <span className="hidden sm:inline">{t('agentchat.files')}</span>
            </Link>
          )}

          {/* Desktop: bordered icon group so Compact/Hide/Clear are obvious */}
          <div className="hidden sm:flex items-center gap-0.5 ml-0.5 rounded-[var(--radius-md)] border border-pc-border bg-pc-elevated p-0.5">
            <Button
              variant="ghost"
              size="sm"
              onClick={toggleCompact}
              className="h-6 w-6 !p-0"
              aria-label={t('agent.compact_mode')}
              title={t('agent.compact_mode')}
            >
              {compact ? <Maximize2 className="h-5 w-5" strokeWidth={2} /> : <Minimize2 className="h-5 w-5" strokeWidth={2} />}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={toggleToolActivity}
              // Pressed = tool activity shown. The icon alone never changes,
              // so the state has to read from the button's own colour.
              className="h-6 w-6 !p-0 aria-pressed:border-pc-accent/40 aria-pressed:bg-pc-accent/15 aria-pressed:text-pc-accent"
              aria-label={showToolActivity ? t('agent.tool_activity_hide') : t('agent.tool_activity_show')}
              aria-pressed={showToolActivity}
              title={showToolActivity ? t('agent.tool_activity_hide') : t('agent.tool_activity_show')}
            >
              <Wrench className="h-5 w-5" strokeWidth={2} />
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={toggleKeepThinkingCollapsed}
              className="h-6 w-6 !p-0 aria-pressed:border-pc-accent/40 aria-pressed:bg-pc-accent/15 aria-pressed:text-pc-accent"
              aria-label={t('agent.thinking_keep_collapsed')}
              aria-pressed={keepThinkingCollapsed}
              title={t('agent.thinking_keep_collapsed')}
            >
              <Brain className="h-5 w-5" strokeWidth={2} />
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={handleClearAll}
              className="h-6 w-6 !p-0 text-status-error hover:text-status-error"
              aria-label={t('agent.clear_all')}
              title={t('agent.clear_all')}
            >
              <Trash2 className="h-5 w-5" strokeWidth={2} />
            </Button>
          </div>

          {/* Mobile: More menu (always available) */}
          <div className="relative sm:hidden" ref={moreMenuRef}>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => setShowMoreMenu((v) => !v)}
              className="h-7 w-7 px-0 border border-pc-border"
              aria-label={t('common.actions')}
              aria-expanded={showMoreMenu}
              title={t('common.actions')}
            >
              <MoreVertical className="h-3.5 w-3.5" />
            </Button>
            {showMoreMenu && (
              <div className="absolute left-0 mt-1.5 z-50 min-w-[180px] rounded-[var(--radius-md)] border border-pc-border bg-pc-elevated py-1 shadow-[var(--pc-shadow-md)]">
                <button
                  type="button"
                  onClick={() => { toggleCompact(); setShowMoreMenu(false); }}
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-pc-text transition-colors hover:bg-[var(--pc-hover)]"
                >
                  {compact ? <Maximize2 className="h-3.5 w-3.5" /> : <Minimize2 className="h-3.5 w-3.5" />}
                  {t('agent.compact_mode')}
                </button>
                <button
                  type="button"
                  onClick={() => { toggleToolActivity(); setShowMoreMenu(false); }}
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-pc-text transition-colors hover:bg-[var(--pc-hover)]"
                  aria-pressed={showToolActivity}
                >
                  <Wrench className={`h-3.5 w-3.5 ${showToolActivity ? 'text-pc-accent' : ''}`} />
                  {showToolActivity ? t('agent.tool_activity_hide') : t('agent.tool_activity_show')}
                  {showToolActivity && <Check className="ml-auto h-3.5 w-3.5 text-pc-accent" aria-hidden="true" />}
                </button>
                <button
                  type="button"
                  onClick={() => { toggleKeepThinkingCollapsed(); setShowMoreMenu(false); }}
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-pc-text transition-colors hover:bg-[var(--pc-hover)]"
                  aria-pressed={keepThinkingCollapsed}
                >
                  <Brain className={`h-3.5 w-3.5 ${keepThinkingCollapsed ? 'text-pc-accent' : ''}`} />
                  {t('agent.thinking_keep_collapsed')}
                  {keepThinkingCollapsed && <Check className="ml-auto h-3.5 w-3.5 text-pc-accent" aria-hidden="true" />}
                </button>
                <button
                  type="button"
                  onClick={() => { handleClearAll(); setShowMoreMenu(false); }}
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-status-error transition-colors hover:bg-[var(--pc-hover)]"
                >
                  <Trash2 className="h-3.5 w-3.5" />
                  {t('agent.clear_all')}
                </button>
              </div>
            )}
          </div>
        </div>

        <div className="flex min-w-0 shrink items-center gap-1.5 sm:gap-2">
          {/* Nothing to list or reopen when each socket is its own conversation. */}
          {!conversationPerSocket && (
            <SessionPicker
              agentAlias={agentAlias}
              labelClassName="max-w-[90px] sm:max-w-[160px]"
            />
          )}

          {operator ? (
            <div className="relative min-w-0" ref={modelDropdownRef}>
              <button
                type="button"
                onClick={() => setShowModelDropdown((v) => !v)}
                disabled={modelLoading || typing || (availableModels.length === 0 && currentModel === null)}
                className="flex max-w-full min-w-0 items-center gap-1.5 sm:gap-2 px-2 sm:px-3 h-7 rounded-[var(--radius-md)] text-xs font-medium border border-pc-border bg-pc-elevated text-pc-text-secondary transition-colors hover:text-pc-text hover:border-pc-border-strong disabled:opacity-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)]"
              >
                <span className="max-w-[90px] sm:max-w-[180px] truncate">
                  {modelLoading
                    ? t('agent.model_switching')
                    : (currentModel ?? (availableModels.length === 0 ? t('agent.model_loading') : t('agent.select_model')))}
                </span>
                <ChevronDown className="h-3 w-3 shrink-0" />
              </button>

              {showModelDropdown && availableModels.length > 0 && (
                <div className="absolute right-0 mt-1.5 rounded-[var(--radius-md)] border border-pc-border bg-pc-elevated shadow-[var(--pc-shadow-md)] z-50 py-1 min-w-[200px] max-h-60 overflow-y-auto">
                  {availableModels.map((model) => {
                    const isActive = model === currentModel;
                    return (
                      <button
                        key={model}
                        type="button"
                        onClick={() => handleModelSwitch(model)}
                        className={`w-full text-left px-3 py-2 text-xs transition-colors ${
                          isActive
                            ? 'text-pc-accent bg-pc-accent/10'
                            : 'text-pc-text hover:bg-[var(--pc-hover)]'
                        }`}
                      >
                        {model}
                      </button>
                    );
                  })}
                </div>
              )}
            </div>
          ) : (
            // Read-only: switching rewrites the agent's config for everyone.
            <span
              data-model-readonly=""
              className="flex max-w-full min-w-0 items-center px-2 sm:px-3 h-7 rounded-[var(--radius-md)] text-xs font-medium border border-pc-border bg-pc-elevated text-pc-text-secondary"
            >
              <span className="max-w-[90px] sm:max-w-[180px] truncate">{currentModel ?? t('agent.model_loading')}</span>
            </span>
          )}
        </div>
      </div>

      {/* Connection status bar */}
      {error && (
        <div className="px-4 py-2 border-b border-status-error/20 bg-status-error/10 text-status-error flex items-start gap-2 text-sm animate-fade-in">
          <AlertCircle className="h-4 w-4 shrink-0 mt-0.5" />
          <p className="min-w-0 flex-1 break-words">{error}</p>
        </div>
      )}
      {conversationLimited && (
        <div
          data-conversation-limit=""
          className="px-4 py-2 border-b border-pc-border bg-pc-surface text-pc-text-secondary flex items-center gap-2 text-sm animate-fade-in"
        >
          <AlertCircle className="h-4 w-4 shrink-0" />
          <span className="flex-1">{t('agent.conversation_limit')}</span>
          <Button size="sm" variant="ghost" onClick={retryConnection}>
            {t('common.retry')}
          </Button>
        </div>
      )}

      {/* Messages area. `min-h-0` lets this flex child shrink below a tall
          image's min-content size, so the overflow stays inside this
          scrollport instead of spilling out and getting clipped.
          `overflow-x-hidden` because `overflow-y: auto` alone computes the
          other axis to `auto`. The content wrapper's `overflow-x-clip` then
          removes the sideways extent itself (a user bubble slides in from
          `translateX(16px)`), so neither a touch nor a focus can move the
          pane sideways. Wide code blocks and tables scroll inside their own
          box. */}
      <div
        ref={scrollerRef}
        className="flex-1 min-h-0 overflow-y-auto overflow-x-hidden"
      >
        <div
          ref={contentRef}
          className={`chat-transcript flex min-h-full flex-col overflow-x-clip ${compact ? 'space-y-1 p-2.5' : 'space-y-2 p-3'}`}
        >
        {messages.length === 0 && (
          <div className="flex flex-1 flex-col items-center justify-center text-center animate-fade-in text-pc-text-muted">
            <div className="h-14 w-14 rounded-[var(--radius-lg)] flex items-center justify-center mb-4 bg-pc-accent/10">
              <Bot className="h-7 w-7 text-pc-accent" />
            </div>
            {/* Until the transcript lands, an empty list means "still loading",
                not "empty conversation" — saying the latter misreports a
                conversation the operator just picked precisely because it has
                history in it. */}
            {hydrated ? (
              <>
                <p className="text-base font-semibold mb-1 text-pc-text">{t('agentchat.empty_title')}</p>
                <p className="text-sm text-pc-text-muted">{t('agent.start_conversation')}</p>
                {conversationPerSocket && (
                  <p data-conversation-scope-note="" className="text-xs mt-2 text-pc-text-faint">
                    {t('agent.conversation_ends_with_page')}
                  </p>
                )}
              </>
            ) : (
              <p className="text-sm text-pc-text-muted">{t('agent.session_loading')}</p>
            )}
          </div>
        )}

        {hasEarlierTurns && (
          <div className="flex justify-center">
            <button
              type="button"
              onClick={() => void handleLoadEarlier()}
              disabled={loadingEarlier}
              aria-label={t('agentchat.load_earlier')}
              className="rounded-[var(--radius-md)] px-3 py-1 text-xs text-pc-text-muted hover:text-pc-text hover:bg-pc-accent/10 disabled:opacity-60"
            >
              {loadingEarlier ? t('agentchat.loading_earlier') : t('agentchat.load_earlier')}
            </button>
          </div>
        )}

        {blocks.map((block, idx) =>
          block.kind === 'group' ? (
            <GroupItem
              key={block.id}
              segments={block.segments}
              idx={idx}
              compact={compact}
              collapsed={!expandedGroups[block.id]}
              onToggle={() => openGroup(block.id, block.segments)}
            />
          ) : (
            <MessageItem
              key={block.message.id}
              msg={block.message}
              idx={idx}
              compact={compact}
              isCopied={copiedId === block.message.id}
              onCopy={handleCopy}
              onDelete={handleDeleteMessage}
              segments={block.kind === 'turn' ? block.segments : undefined}
              groupCollapsed={!expandedGroups[block.message.id]}
              onToggleGroup={
                block.kind === 'turn'
                  ? () => openGroup(block.message.id, block.segments)
                  : undefined
              }
            />
          ),
        )}

        {typing && (
          <div className={`flex items-start animate-fade-in ${compact ? 'gap-1.5' : 'gap-2'}`}>
            {!compact && (
              <div className="flex-shrink-0 w-6 h-6 rounded-[var(--radius-md)] flex items-center justify-center border border-pc-border bg-pc-elevated">
                <Bot className="h-3.5 w-3.5 text-pc-accent" />
              </div>
            )}
            {/* An image in the streaming trajectory group is rendered by the
                same `img` renderer, so it needs the same width rule as a
                committed message. */}
            <div className={`relative min-w-0 ${stepsHaveChatImage(live.groupSteps) ? 'msg-has-image max-w-[85%]' : 'max-w-[85%]'}`}>
              {/* The trajectory so far, above the answer still arriving: the
                  committed layout, so committing the turn moves nothing. */}
              {showToolActivity && live.groupSteps.length > 0 && (
                <ToolCallGroup
                  segments={{ steps: live.groupSteps }}
                  compact={compact}
                  collapsed={liveGroupCollapsed}
                  onToggle={toggleLiveGroup}
                />
              )}
              {live.answer ? (
                <div className={`${compact ? 'rounded-[var(--radius-md)] px-2 py-1' : 'rounded-[var(--radius-md)] px-2.5 py-1.5'} border border-pc-border bg-pc-elevated text-pc-text`}>
                  {live.answer.thinking && (
                    <details className="mb-1" open={keepThinkingCollapsed ? undefined : !live.answer.text}>
                      <summary className="text-xs cursor-pointer select-none text-pc-text-muted">{t('agentchat.thinking')}{!live.answer.text && '...'}</summary>
                      <pre className="text-xs mt-1 whitespace-pre-wrap break-words leading-snug overflow-auto max-h-60 p-1.5 rounded-[var(--radius-sm)] text-pc-text-muted bg-pc-code">{live.answer.thinking}</pre>
                    </details>
                  )}
                  {live.answer.text && <p className={`${compact ? 'text-xs' : 'text-sm'} whitespace-pre-wrap break-words leading-snug`}>{live.answer.text}</p>}
                </div>
              ) : (
                <div className="rounded-[var(--radius-md)] px-2.5 py-1.5 border border-pc-border bg-pc-elevated flex items-center gap-1.5">
                  <span className="bounce-dot w-1.5 h-1.5 rounded-full bg-pc-accent" />
                  <span className="bounce-dot w-1.5 h-1.5 rounded-full bg-pc-accent" />
                  <span className="bounce-dot w-1.5 h-1.5 rounded-full bg-pc-accent" />
                </div>
              )}
            </div>
          </div>
        )}
        </div>
      </div>

      {/* Tool approval banner — supervised-mode consent prompt (#6522). */}
      {pendingApproval && (
        <ApprovalBanner pending={pendingApproval} onRespond={respondToApproval} />
      )}

      {/* Input area */}
      <div className="border-t border-pc-border bg-pc-surface p-4">
        {/* Slash-command autocomplete popover (#7137) */}
        {showCommandHint && matchedCommands.length > 0 && (
          <div className="relative max-w-4xl mx-auto">
            <div
              className="absolute bottom-1 left-0 rounded-xl border shadow-lg z-50 py-1 min-w-[260px] overflow-hidden"
              style={{ background: 'var(--pc-bg-elevated)', borderColor: 'var(--pc-border)' }}
            >
              <div
                className="px-3 py-1 text-[10px] uppercase tracking-wide"
                style={{ color: 'var(--pc-text-faint)' }}
              >
                {t('agent.cmd_hint_title')}
              </div>
              {matchedCommands.map((spec) => (
                <button
                  key={spec.name}
                  type="button"
                  onMouseDown={(e) => { e.preventDefault(); applyCommandHint(spec); }}
                  className="w-full text-left px-3 py-2 text-xs transition-colors flex items-center gap-2"
                  style={{ color: 'var(--pc-text-primary)' }}
                  onMouseEnter={(e) => { e.currentTarget.style.background = 'var(--pc-bg-surface)'; }}
                  onMouseLeave={(e) => { e.currentTarget.style.background = 'transparent'; }}
                >
                  <span className="font-mono font-medium" style={{ color: 'var(--pc-accent)' }}>{spec.usage}</span>
                  <span className="truncate" style={{ color: 'var(--pc-text-muted)' }}>{t(spec.descriptionKey)}</span>
                </button>
              ))}
            </div>
          </div>
        )}
        {/* Composer. See InputBar.tsx for the visual rationale. */}
        <InputBar
          connected={connected}
          canSend={canSend}
          canAttach={operator}
          startsConversationOnSend={startsConversationOnSend}
          hydrated={hydrated}
          typing={typing}
          uploading={uploading}
          input={input}
          inputRef={inputRef}
          fileInputRef={fileInputRef}
          contextInputTokens={contextInputTokens}
          contextEffectiveWindow={contextEffectiveWindow}
          contextTrimBudgetTokens={contextTrimBudgetTokens}
          contextInputSource={contextInputSource}
          liveStats={liveStats}
          onInputChange={handleTextareaChange}
          onKeyDown={handleKeyDown}
          onCompositionStart={() => { isComposingRef.current = true; }}
          onCompositionEnd={() => { isComposingRef.current = false; }}
          onSend={handleSend}
          onAbort={handleAbort}
          onUpload={uploadImages}
        />
      </div>
    </div>
  );
}

// Each chat message is rendered through this memoized component so that
// typing into the input does not re-render every existing message (and
// re-run ReactMarkdown on each one). Keep the prop surface small and pass
// `isCopied` rather than the parent's full copiedId so only the affected
// row re-renders when the copy indicator flips. See #5125.
interface MessageItemProps {
  msg: ChatMessage;
  idx: number;
  compact: boolean;
  isCopied: boolean;
  onCopy: (id: string, content: string) => void;
  onDelete: (id: string) => void;
  /** The turn's step trajectory, when this message is a turn's answer — live
   *  or derived from a reloaded turn's bubbles. Rendered as a collapsed group
   *  above the answer; the answer itself never goes inside the group. */
  segments?: TurnSegments;
  /** Whether that group is collapsed. */
  groupCollapsed?: boolean;
  onToggleGroup?: () => void;
}

/** Format bubble timestamps as `yy-MM-dd HH:mm:ss` (local time). */
function formatMsgTime(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  const yy = String(d.getFullYear()).slice(-2);
  return `${yy}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

const MessageItem = memo(function MessageItem({
  msg,
  idx,
  compact,
  isCopied,
  onCopy,
  onDelete,
  segments,
  groupCollapsed,
  onToggleGroup,
}: MessageItemProps) {
  // Locally-composed user input and locally-generated command output are
  // verbatim and never carry the gateway's `[timestamp]` prefix, so don't strip
  // them (that would clip a message starting with a bracketed datetime). Only
  // server-sourced messages can be prefixed.
  const cleanContent = msg.local || msg.ephemeral ? msg.content : stripServerTimestamp(msg.content);
  // An image in the answer, or in the trajectory group rendered above the
  // answer, is sized by the same `img` renderer, so both make the bubble
  // image-bearing (the mobile full-width rule in `index.css` keys on this).
  const hasImage = hasChatImage(cleanContent) || stepsHaveChatImage(segments?.steps ?? []);

  // The hover strip below the bubble is absolutely positioned, so it needs
  // reserved space to land in: it is 20px tall (18px bar + 2px offset) while
  // `space-y-*` only opens 8px — 4px in compact mode — between rows, so the
  // strip used to cover the top of the next message. The bottom padding makes
  // up the difference instead. Compact keeps its 4px edge: there the strip
  // still grazes the next bubble's top border, which is above its text.
  return (
    <div
      className={`group relative hover:z-20 flex items-start pb-3 ${compact ? 'gap-1.5' : 'gap-2'} ${
        msg.role === 'user' ? 'flex-row-reverse animate-slide-in-right' : 'animate-slide-in-left'
      }`}
      style={{ animationDelay: `${Math.min(idx * 30, 200)}ms` }}
    >
      {!compact && (
        <div
          className={`flex-shrink-0 w-6 h-6 rounded-[var(--radius-md)] flex items-center justify-center border ${
            msg.notice
              ? 'bg-status-warning/10 border-status-warning/30'
              : msg.role === 'user'
              ? 'bg-pc-accent/15 border-pc-accent/30'
              : 'bg-pc-elevated border-pc-border'
          }`}
        >
          {msg.notice ? (
            <AlertCircle className="h-3.5 w-3.5 text-status-warning" />
          ) : msg.role === 'user' ? (
            <User className="h-3.5 w-3.5 text-pc-accent" />
          ) : (
            <Bot className="h-3.5 w-3.5 text-pc-accent" />
          )}
        </div>
      )}
      <div className={`relative min-w-0 overflow-visible ${hasImage ? 'msg-has-image max-w-[85%]' : 'max-w-[85%]'}`}>
        {segments && segments.steps.length > 0 && onToggleGroup && (
          <ToolCallGroup
            segments={segments}
            compact={compact}
            collapsed={groupCollapsed !== false}
            onToggle={onToggleGroup}
          />
        )}
        <div
          className={`relative ${compact ? 'rounded-[var(--radius-md)] px-2 py-1' : 'rounded-[var(--radius-md)] px-2.5 py-1.5'} border text-pc-text ${
            msg.notice
              ? 'bg-status-warning/5 border-status-warning/30'
              : msg.role === 'user'
              ? 'bg-pc-accent/10 border-pc-accent/20'
              : 'bg-pc-elevated border-pc-border'
          }`}
          title={!msg.toolCall ? formatMsgTime(msg.timestamp) : undefined}
        >
          {msg.thinking && (
            <details className="mb-1">
              <summary className="text-xs cursor-pointer select-none text-pc-text-muted">{t('agentchat.thinking')}</summary>
              <pre className="text-xs mt-1 whitespace-pre-wrap break-words leading-snug overflow-auto max-h-60 p-1.5 rounded-[var(--radius-sm)] text-pc-text-muted bg-pc-code">{msg.thinking}</pre>
            </details>
          )}
          {msg.toolCall ? (
            <ToolCallCard toolCall={msg.toolCall} />
          ) : msg.markdown ? (
            <div className={`${compact ? 'text-xs' : 'text-sm'} break-words leading-snug chat-markdown chat-markdown-dense`}><ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>{cleanContent}</ReactMarkdown></div>
          ) : (
            <p className={`${compact ? 'text-xs' : 'text-sm'} whitespace-pre-wrap break-words leading-snug`}>{cleanContent}</p>
          )}
        </div>
        {/* Below the bubble, anchored inward (not toward the screen edge) so short
            left/right bubbles do not clip the strip. Upward hit-bridge keeps
            hover continuous when moving onto the buttons. */}
        <div
          className={`absolute top-full z-10 ${
            msg.role === 'user' ? 'right-0' : 'left-0'
          } opacity-0 transition-opacity duration-100 group-hover:opacity-100`}
        >
          <div className="absolute bottom-full left-0 right-0 h-6" aria-hidden />
          <div
            className="mt-0.5 flex items-center gap-1 whitespace-nowrap rounded-[var(--radius-sm)] border border-pc-border px-1"
            style={{
              background: 'color-mix(in srgb, var(--pc-bg-elevated) 94%, transparent)',
            }}
          >
            {!msg.toolCall && (
              <span className="select-none text-[10px] leading-none tabular-nums text-pc-text-muted">
                {formatMsgTime(msg.timestamp)}
              </span>
            )}
            <button
              type="button"
              onClick={() => onCopy(msg.id, cleanContent)}
              aria-label={t('agent.copy_message')}
              className="rounded-[var(--radius-sm)] p-0.5 text-pc-text-muted transition-colors hover:text-pc-text"
            >
              {isCopied ? (
                <Check className="h-3 w-3 text-status-success" />
              ) : (
                <Copy className="h-3 w-3" />
              )}
            </button>
            <button
              type="button"
              onClick={() => onDelete(msg.id)}
              aria-label={t('agent.delete_message')}
              className="rounded-[var(--radius-sm)] p-0.5 text-pc-text-muted transition-colors hover:text-status-error"
            >
              <X className="h-3 w-3" />
            </button>
          </div>
        </div>
      </div>
    </div>
  );
});

/** A turn with tool calls and no answer bubble (stopped, failed, or ended on
 *  a tool call): the group alone, in the agent row layout. No copy/delete
 *  strip — there is no bubble to act on. */
const GroupItem = memo(function GroupItem({
  segments,
  idx,
  compact,
  collapsed,
  onToggle,
}: {
  segments: TurnSegments;
  idx: number;
  compact: boolean;
  collapsed: boolean;
  onToggle: () => void;
}) {
  return (
    <div
      className={`flex items-start pb-3 animate-slide-in-left ${compact ? 'gap-1.5' : 'gap-2'}`}
      style={{ animationDelay: `${Math.min(idx * 30, 200)}ms` }}
    >
      {!compact && (
        <div className="flex-shrink-0 w-6 h-6 rounded-[var(--radius-md)] flex items-center justify-center border bg-pc-elevated border-pc-border">
          <Bot className="h-3.5 w-3.5 text-pc-accent" />
        </div>
      )}
      <div className={`relative min-w-0 ${stepsHaveChatImage(segments.steps) ? 'msg-has-image max-w-[85%]' : 'max-w-[85%]'}`}>
        <ToolCallGroup segments={segments} compact={compact} collapsed={collapsed} onToggle={onToggle} />
      </div>
    </div>
  );
});
