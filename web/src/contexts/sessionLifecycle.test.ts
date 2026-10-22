import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';
import React, { createElement } from 'react';
import {
  act,
  create,
  type ReactTestInstance,
  type ReactTestRenderer,
} from 'react-test-renderer';
import type { ApprovalDecision, SessionMessagesResponse, WsMessage } from '../types/api.ts';
import type {
  AgentContextValue,
  AgentSessionRuntime,
  SessionSocket,
} from '@/contexts/AgentContext';

class MemoryStorage implements Storage {
  private readonly values = new Map<string, string>();

  get length(): number { return this.values.size; }
  clear(): void { this.values.clear(); }
  getItem(key: string): string | null { return this.values.get(key) ?? null; }
  key(index: number): string | null { return [...this.values.keys()][index] ?? null; }
  removeItem(key: string): void { this.values.delete(key); }
  setItem(key: string, value: string): void { this.values.set(key, value); }
}

const storage = new MemoryStorage();
const fakeWindow = {
  location: { protocol: 'http:', host: 'localhost' },
  dispatchEvent: () => true,
};
const fakeDocument = {
  addEventListener: () => {},
  removeEventListener: () => {},
  createElement: () => ({
    style: {},
    focus: () => {},
    select: () => {},
    value: '',
  }),
  body: { appendChild: () => {}, removeChild: () => {} },
  execCommand: () => true,
};

Object.assign(globalThis, {
  React,
  localStorage: storage,
  window: fakeWindow,
  document: fakeDocument,
});
// The chat pane re-pins its scroll position on the frame after a content
// growth, so `requestAnimationFrame` must exist for a mount to complete. Run
// the callback on a microtask: these tests assert what is rendered, not where
// the pane scrolled, and a synchronous call would make the effect re-enter
// mid-commit.
Object.assign(globalThis, {
  requestAnimationFrame: (cb: FrameRequestCallback): number =>
    Number(setTimeout(() => cb(Date.now()), 0)),
  cancelAnimationFrame: (handle: number): void => clearTimeout(handle),
});
// Observed rather than exercised: jsdom-less react-test-renderer has no layout,
// so the callback never fires. It only has to be constructible for the pane to
// mount.
class NoopResizeObserver implements ResizeObserver {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
}
Object.assign(globalThis, { ResizeObserver: NoopResizeObserver });
Object.defineProperty(globalThis, 'navigator', {
  configurable: true,
  value: { language: 'en-US', clipboard: { writeText: async () => {} } },
});
(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

let listedSessions: Array<Record<string, unknown>> = [];
type SessionsResponder = () => Promise<Array<Record<string, unknown>>>;
let sessionsResponder: SessionsResponder | null = null;
interface ConfigPutRequest {
  path: string;
  value: unknown;
  comment?: string;
}
let configPutCalls: ConfigPutRequest[] = [];
/** Every `/api/sessions*` request the tree makes. The composer's telemetry is
 *  built from WebSocket frames only, so a turn must add none of these. */
let sessionRequests: string[] = [];
// Session ids the mocked gateway reports as already gone / failing on DELETE.
let missingSessions = new Set<string>();
let deleteFailures = new Set<string>();
let configPutHandler: ((request: ConfigPutRequest) => Promise<Response>) | null = null;

globalThis.fetch = async (input, init) => {
  const url = typeof input === 'string'
    ? input
    : input instanceof URL
      ? input.toString()
      : input.url;
  if (url.includes('/api/sessions')) sessionRequests.push(url);
  let body: unknown;
  if (url.includes('/api/config/catalog')) body = { providers: [] };
  else if (url.includes('/api/status')) body = { model: 'test-model' };
  else if (url.includes('/api/config/prop') && init?.method === 'PUT') {
    const request = JSON.parse(String(init.body)) as ConfigPutRequest;
    configPutCalls.push(request);
    if (configPutHandler) return configPutHandler(request);
    body = { path: request.path, value: request.value };
  } else if (url.includes('/api/config/prop')) body = { path: '', value: '<unset>' };
  else if (url.includes('/api/config/list')) body = { entries: [] };
  else if (url.endsWith('/api/sessions')) {
    // `sessionsResponder` lets a test control when each listing resolves, so
    // out-of-order list responses can be reproduced deterministically.
    body = { sessions: sessionsResponder ? await sessionsResponder() : listedSessions };
  }
  else if (/\/api\/sessions\/[^/]+$/.test(url) && init?.method === 'DELETE') {
    // The gateway's real DELETE contract: a plain `{"error": ...}` body, not
    // the structured ConfigApiError envelope, on a missing row.
    const id = decodeURIComponent(url.slice(url.lastIndexOf('/') + 1));
    if (deleteFailures.has(id)) {
      return new Response('{"error":"backend failure"}', { status: 500 });
    }
    if (missingSessions.has(id)) {
      return new Response('{"error":"Session not found"}', { status: 404 });
    }
    body = { deleted: true, session_id: id };
  }
  else return new Response('{"error":"not found"}', { status: 404 });
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
};

// Import through `@/` so jiti resolves the same module identity as AgentChat
// (which also uses the alias). Relative vs alias paths otherwise load two
// copies of AgentContext / api, breaking Provider identity and `instanceof`.
const { AgentProvider, useAgent } = await import('@/contexts/AgentContext');
const { deleteSession, HttpError, ApiError } = await import('@/lib/api');
const { DraftContext } = await import('@/hooks/useDraft');
const { AgentChatInner } = await import('@/pages/AgentChat');
const { MemoryRouter } = await import('react-router-dom');

class Deferred<T> {
  readonly promise: Promise<T>;
  private resolvePromise!: (value: T) => void;
  private rejectPromise!: (reason: unknown) => void;

  constructor() {
    this.promise = new Promise<T>((resolve, reject) => {
      this.resolvePromise = resolve;
      this.rejectPromise = reject;
    });
  }

  resolve(value: T): void { this.resolvePromise(value); }
  reject(reason: unknown): void { this.rejectPromise(reason); }
}

class FakeSocket implements SessionSocket {
  onMessage: ((msg: WsMessage) => void) | null = null;
  onOpen: (() => void) | null = null;
  onClose: ((ev: CloseEvent) => void) | null = null;
  onError: ((ev: Event) => void) | null = null;
  connectCalls = 0;
  disconnectCalls = 0;
  sent: string[] = [];
  approvalResponses: Array<{ requestId: string; decision: ApprovalDecision }> = [];
  private open = false;

  constructor(
    readonly agentAlias: string,
    readonly sessionId: string,
  ) {}

  connect(): void { this.connectCalls += 1; }
  disconnect(): void { this.disconnectCalls += 1; this.open = false; }
  get connected(): boolean { return this.open; }
  sendMessage(content: string): void {
    if (!this.open) throw new Error('closed');
    this.sent.push(content);
  }
  sendApprovalResponse(requestId: string, decision: ApprovalDecision): void {
    this.approvalResponses.push({ requestId, decision });
  }
  emitOpen(): void { this.open = true; this.onOpen?.(); }
  emitClose(code = 1006): void {
    this.open = false;
    this.onClose?.({ code } as CloseEvent);
  }
  emitMessage(message: WsMessage): void { this.onMessage?.(message); }
}

type MessageFactory = () => Promise<SessionMessagesResponse>;
type DeleteFactory = () => Promise<{ deleted: boolean }>;

class FakeSessionRuntime implements AgentSessionRuntime {
  readonly sockets: FakeSocket[] = [];
  readonly deleteCalls: string[] = [];
  readonly renameCalls: Array<{ id: string; name: string }> = [];
  readonly messageCalls: string[] = [];
  readonly messagePlans = new Map<string, MessageFactory[]>();
  readonly deletePlans = new Map<string, DeleteFactory[]>();
  readonly renameDeferred = new Map<string, Deferred<{ session_id: string; name: string }>>();
  mintedIds: string[] = [];

  createSocket({ agentAlias, sessionId }: { agentAlias: string; sessionId: string }): FakeSocket {
    const socket = new FakeSocket(agentAlias, sessionId);
    this.sockets.push(socket);
    return socket;
  }

  getMessages(sessionId: string): Promise<SessionMessagesResponse> {
    this.messageCalls.push(sessionId);
    const plan = this.messagePlans.get(sessionId)?.shift();
    return plan ? plan() : Promise.resolve(messagesResponse(sessionId, true));
  }

  delete(sessionId: string): Promise<{ deleted: boolean }> {
    this.deleteCalls.push(sessionId);
    const plan = this.deletePlans.get(sessionId)?.shift();
    return plan ? plan() : Promise.resolve({ deleted: true });
  }

  rename(sessionId: string, name: string): Promise<{ session_id: string; name: string }> {
    this.renameCalls.push({ id: sessionId, name });
    return this.renameDeferred.get(sessionId)?.promise
      ?? Promise.resolve({ session_id: sessionId, name });
  }

  mintId(): string {
    const id = this.mintedIds.shift();
    if (!id) throw new Error('No deterministic session id queued');
    return id;
  }

  queueMessages(sessionId: string, plan: MessageFactory): void {
    const plans = this.messagePlans.get(sessionId) ?? [];
    plans.push(plan);
    this.messagePlans.set(sessionId, plans);
  }

  queueDelete(sessionId: string, plan: DeleteFactory): void {
    const plans = this.deletePlans.get(sessionId) ?? [];
    plans.push(plan);
    this.deletePlans.set(sessionId, plans);
  }
}

function messagesResponse(
  sessionId: string,
  sessionPersistence: boolean,
  contents: string[] = [],
): SessionMessagesResponse {
  return {
    session_id: sessionId,
    session_persistence: sessionPersistence,
    messages: contents.map((content) => ({ role: 'user', content, created_at: null })),
  };
}

interface MountedChat {
  renderer: ReactTestRenderer;
  context(): AgentContextValue;
  drafts: Map<string, string>;
  /// A sample host node, so tests can assert the mock's surface.
  hostProbe(): Record<string, unknown>;
}

/// The stand-in host node for one rendered element.
///
/// Host nodes in this renderer are not DOM nodes, so the tree can only read
/// what this returns. The contract test below walks the mounted pane and
/// asserts nothing it touches is missing here — without that, a component
/// adopting a new DOM surface (e.g. geometry for the follow-the-tail
/// decision) throws during mount and takes the whole file down at once.
function hostNodeMock(type: string): Record<string, unknown> {
  if (type === 'textarea') {
    return { style: {}, focus: () => {}, scrollHeight: 24 };
  }
  return {
    focus: () => {},
    scrollIntoView: () => {},
    contains: () => false,
    addEventListener: () => {},
    removeEventListener: () => {},
    scrollTop: 0,
    scrollHeight: 0,
    clientHeight: 0,
    // The transcript re-pins by observing the content wrapper, and it reads
    // its height before the observer ever fires. Height 0 is honest here:
    // this renderer has no layout, so the pane never grows and the callback
    // never re-pins.
    getBoundingClientRect: () => ({
      width: 0,
      height: 0,
      top: 0,
      left: 0,
      bottom: 0,
      right: 0,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    }),
  };
}

async function mountChat(
  runtime: FakeSessionRuntime,
  includeChat = false,
  reservedSessionIds?: readonly string[],
): Promise<MountedChat> {
  let currentContext: AgentContextValue | null = null;
  const drafts = new Map<string, string>();
  const draftStore = {
    getDraft: (key: string) => drafts.get(key) ?? '',
    setDraft: (key: string, value: string) => { drafts.set(key, value); },
    clearDraft: (key: string) => { drafts.delete(key); },
  };

  function Probe() {
    currentContext = useAgent();
    return null;
  }

  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(
      createElement(
        MemoryRouter,
        null,
        createElement(
          AgentProvider,
          {
            agentAlias: 'ops',
            sessionRuntime: runtime,
            reservedSessionIds,
            children: createElement(
              React.Fragment,
              null,
              createElement(Probe),
              includeChat
                ? createElement(
                  DraftContext.Provider,
                  { value: draftStore },
                  createElement(AgentChatInner, { agentAlias: 'ops' }),
                )
                : null,
            ),
          },
        ),
      ),
      {
        // Host nodes in this renderer are stand-ins, so they must expose every
        // DOM surface the tree touches: the composer textarea, the transcript
        // scroller (scroll listeners + geometry for the follow-the-tail
        // decision), and the generic node API used by the remaining widgets.
        createNodeMock: (element) =>
          hostNodeMock(typeof element.type === 'string' ? element.type : 'div'),
      },
    );
    await Promise.resolve();
  });

  return {
    renderer,
    context: () => {
      if (!currentContext) throw new Error('Probe did not render');
      return currentContext;
    },
    drafts,
    hostProbe: () => hostNodeMock('div'),
  };
}

async function settle(): Promise<void> {
  await act(async () => {
    await Promise.resolve();
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  });
}

function nodeText(node: ReactTestInstance): string {
  return node.children
    .map((child) => typeof child === 'string' ? child : nodeText(child))
    .join('');
}

function textarea(renderer: ReactTestRenderer): ReactTestInstance {
  return renderer.root.findByType('textarea');
}

async function typeInComposer(renderer: ReactTestRenderer, value: string): Promise<void> {
  await act(async () => {
    textarea(renderer).props.onChange({
      target: { value, style: {}, scrollHeight: 24 },
    });
  });
}

async function openSocket(runtime: FakeSessionRuntime, index: number): Promise<void> {
  await act(async () => { runtime.sockets[index]!.emitOpen(); });
}

async function goToSession(mounted: MountedChat, sessionId: string): Promise<boolean> {
  let accepted = false;
  await act(async () => {
    accepted = mounted.context().goToSession(sessionId);
  });
  return accepted;
}

async function unmount(renderer: ReactTestRenderer): Promise<void> {
  await act(async () => { renderer.unmount(); });
}

beforeEach(() => {
  storage.clear();
  configPutCalls = [];
  missingSessions = new Set();
  deleteFailures = new Set();
  configPutHandler = null;
  sessionsResponder = null;
  storage.setItem('zeroclaw_active_session.ops', 'A');
  listedSessions = [
    {
      session_id: 'A', session_key: 'gw_A', name: 'First', message_count: 0,
      last_activity: '2026-08-05T00:00:00Z', created_at: '2026-08-05T00:00:00Z',
      agent_alias: 'ops', channel_id: null,
    },
    {
      session_id: 'B', session_key: 'gw_B', name: 'Second', message_count: 0,
      last_activity: '2026-08-04T00:00:00Z', created_at: '2026-08-04T00:00:00Z',
      agent_alias: 'ops', channel_id: null,
    },
  ];
});

for (const scenario of ['unknown', 'disabled'] as const) {
  test(`picker and /new fail closed when persistence is ${scenario}`, async () => {
    const runtime = new FakeSessionRuntime();
    runtime.queueMessages('A', scenario === 'unknown'
      ? () => Promise.reject(new Error('hydration failed'))
      : () => Promise.resolve(messagesResponse('A', false)));
    const mounted = await mountChat(runtime, true);
    await openSocket(runtime, 0);
    await settle();

    assert.equal(mounted.context().hydrated, true);
    assert.equal(mounted.context().sessionPersistence, scenario === 'unknown' ? null : false);

    const trigger = mounted.renderer.root.findAllByType('button')
      .find((button) => button.props.title === 'Conversations');
    assert.ok(trigger);
    await act(async () => { trigger.props.onClick(); });
    await settle();

    const buttons = mounted.renderer.root.findAllByType('button');
    assert.equal(buttons.some((button) => nodeText(button).includes('New conversation')), false);
    // Unknown storage must not be reported as confirmed-disabled storage.
    const notice = mounted.renderer.root.findAllByType('p').map(nodeText).join(' | ');
    assert.equal(notice.includes('does not store conversations'), scenario === 'disabled');
    assert.equal(notice.includes('session storage is confirmed'), scenario === 'unknown');
    const second = buttons.find((button) => nodeText(button).includes('Second'));
    assert.equal(second?.props.disabled, true);

    await typeInComposer(mounted.renderer, '/new');
    const send = mounted.renderer.root.findAllByType('button')
      .find((button) => button.props['aria-label'] === 'Send');
    assert.ok(send);
    await act(async () => { send.props.onClick(); });

    assert.equal(mounted.context().sessionId, 'A');
    assert.equal(storage.getItem('zeroclaw_active_session.ops'), 'A');
    assert.equal(runtime.sockets.length, 1);
    assert.ok(mounted.context().messages.some((message) =>
      message.content.includes('session storage is confirmed')));
    await unmount(mounted.renderer);
  });
}

// The mount's `createNodeMock` is the contract between this harness and the
// pane: host nodes are stand-ins, so every DOM surface the tree touches must
// be listed there. A component that starts reading a new one (e.g. geometry
// via `getBoundingClientRect`) throws during mount, which fails *every* test
// in this file at once — a confusing blast radius for a one-line omission.
// This asserts the mock keeps pace by rendering the pane and walking its host
// instances against the mock's own key set.
test('the mount mock covers every host surface the pane touches', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  const mounted = await mountChat(runtime, true);
  await openSocket(runtime, 0);
  await settle();

  const mockKeys = new Set(Object.keys(mounted.hostProbe()));
  const missing = new Set<string>();
  for (const node of mounted.renderer.root.findAll(() => true)) {
    if (typeof node.type !== 'string') continue;
    for (const key of Object.keys(node.instance ?? {})) {
      // `style` is written by the pane, not read from the host node.
      if (key === 'style') continue;
      if (!mockKeys.has(key)) missing.add(`${node.type}.${key}`);
    }
  }
  assert.deepEqual([...missing], [], 'the mount mock must expose every host surface the pane touches');
  await unmount(mounted.renderer);
});
test('selecting the active picker row closes the menu', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  const mounted = await mountChat(runtime, true);
  await openSocket(runtime, 0);
  await settle();

  const trigger = mounted.renderer.root.findAllByType('button')
    .find((button) => button.props.title === 'Conversations');
  assert.ok(trigger);
  await act(async () => { trigger.props.onClick(); });
  await settle();
  assert.equal(
    mounted.renderer.root.findAllByType('button')
      .find((button) => button.props.title === 'Conversations')?.props['aria-expanded'],
    true,
  );

  const activeRow = mounted.renderer.root.findAllByType('button')
    .find((button) => button.props.title !== 'Conversations' && nodeText(button).includes('First'));
  assert.ok(activeRow);
  await act(async () => { activeRow.props.onClick(); });

  assert.equal(mounted.context().sessionId, 'A');
  assert.equal(
    mounted.renderer.root.findAllByType('button')
      .find((button) => button.props.title === 'Conversations')?.props['aria-expanded'],
    false,
  );
  await unmount(mounted.renderer);
});

test('a conversation another pane owns can be neither opened nor deleted here', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  // A sibling pane of the same agent is live on conversation B.
  const mounted = await mountChat(runtime, true, ['B']);
  await openSocket(runtime, 0);
  await settle();

  assert.deepEqual([...mounted.context().reservedSessionIds], ['B']);

  const trigger = mounted.renderer.root.findAllByType('button')
    .find((button) => button.props.title === 'Conversations');
  assert.ok(trigger);
  await act(async () => { trigger.props.onClick(); });
  await settle();

  const buttons = () => mounted.renderer.root.findAllByType('button');
  const reservedRow = buttons()
    .find((button) => button.props.title === 'Open in another tab' && nodeText(button).includes('Second'));
  assert.ok(reservedRow, 'the sibling-owned row must render');
  assert.equal(reservedRow.props.disabled, true);

  // Clicking it anyway must not move this pane onto the sibling's conversation:
  // two sockets on one gateway session diverge and can abort each other.
  await act(async () => { reservedRow.props.onClick(); });
  await settle();
  assert.equal(mounted.context().sessionId, 'A');
  assert.equal(runtime.sockets.length, 1);

  const reservedDelete = buttons()
    .find((button) => button.props['aria-label'] === 'Delete conversation: Second');
  assert.ok(reservedDelete, 'the sibling-owned row must still offer a delete affordance');
  assert.equal(reservedDelete.props.disabled, true);
  await act(async () => { reservedDelete.props.onClick(); });
  await settle();
  assert.deepEqual(runtime.deleteCalls, []);

  // A conversation no sibling holds stays fully operable from this pane.
  const freeRow = buttons()
    .find((button) => button.props.title !== 'Conversations' && nodeText(button).includes('First'));
  assert.ok(freeRow);
  assert.equal(freeRow.props.disabled, false);
  const freeDelete = buttons()
    .find((button) => button.props['aria-label'] === 'Delete conversation: First');
  assert.equal(freeDelete?.props.disabled, false);

  await unmount(mounted.renderer);
});

test('switch resets capability, hydrates the target, and ignores the old socket', async () => {
  const runtime = new FakeSessionRuntime();
  const bHydration = new Deferred<SessionMessagesResponse>();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true, ['from A'])));
  runtime.queueMessages('B', () => bHydration.promise);
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  assert.equal(mounted.context().sessionId, 'B');
  assert.deepEqual(mounted.context().messages, []);
  assert.equal(mounted.context().hydrated, false);
  assert.equal(mounted.context().sessionPersistence, null);
  assert.equal(runtime.sockets[0]?.disconnectCalls, 1);
  assert.equal(runtime.sockets[1]?.sessionId, 'B');

  await openSocket(runtime, 1);
  await act(async () => {
    runtime.sockets[0]!.emitMessage({ type: 'message', content: 'late A' });
    runtime.sockets[0]!.emitOpen();
    bHydration.resolve(messagesResponse('B', true, ['from B']));
  });
  await settle();

  assert.equal(mounted.context().sessionId, 'B');
  assert.equal(mounted.context().sessionPersistence, true);
  assert.equal(mounted.context().hydrated, true);
  assert.deepEqual(mounted.context().messages.map((message) => message.content), ['from B']);
  await unmount(mounted.renderer);
});

test('a deferred model PUT rebuilds the latest selected session socket', async () => {
  const runtime = new FakeSessionRuntime();
  const configPut = new Deferred<Response>();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('B', () => Promise.resolve(messagesResponse('B', true)));
  configPutHandler = () => configPut.promise;
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  let pendingSwitch!: Promise<void>;
  await act(async () => {
    pendingSwitch = mounted.context().switchModel('kilo.new-model');
    await Promise.resolve();
  });
  assert.deepEqual(configPutCalls, [{
    path: 'agents.ops.model_provider',
    value: 'kilo.new-model',
  }]);
  assert.equal(mounted.context().modelLoading, true);

  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  assert.equal(runtime.sockets[1]?.sessionId, 'B');
  await openSocket(runtime, 1);
  await settle();
  // This socket was constructed before the config write committed, so merely
  // opening it must not claim the model switch succeeded.
  assert.equal(mounted.context().modelLoading, true);

  await act(async () => {
    configPut.resolve(new Response(JSON.stringify({
      path: 'agents.ops.model_provider',
      value: 'kilo.new-model',
    }), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    }));
    await pendingSwitch;
  });
  await settle();

  assert.deepEqual(runtime.sockets.map((socket) => socket.sessionId), ['A', 'B', 'B']);
  assert.ok(runtime.sockets[1]!.disconnectCalls > 0);
  assert.equal(runtime.sockets[2]!.connectCalls, 1);
  assert.equal(mounted.context().sessionId, 'B');
  assert.equal(mounted.context().modelLoading, true);

  await openSocket(runtime, 2);
  assert.equal(mounted.context().modelLoading, false);
  await unmount(mounted.renderer);
});

test('manual rename is the only name write when the first message is sent', async () => {
  const runtime = new FakeSessionRuntime();
  const rename = new Deferred<{ session_id: string; name: string }>();
  runtime.renameDeferred.set('A', rename);
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  const pendingRename = mounted.context().renameConversation('A', 'Release planning');
  await act(async () => { mounted.context().sendMessage('Draft the rollout'); });
  assert.deepEqual(runtime.renameCalls, [{ id: 'A', name: 'Release planning' }]);
  assert.deepEqual(runtime.sockets[0]?.sent, ['Draft the rollout']);

  await act(async () => {
    rename.resolve({ session_id: 'A', name: 'Release planning' });
    await pendingRename;
  });
  assert.equal(runtime.renameCalls.length, 1);
  await unmount(mounted.renderer);
});

test('a listing that lands after an active delete cannot resurrect the deleted row', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.mintedIds.push('C');
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('C', () => Promise.resolve(messagesResponse('C', true)));
  const mounted = await mountChat(runtime, true);
  await openSocket(runtime, 0);
  await settle();

  const buttons = () => mounted.renderer.root.findAllByType('button');
  const trigger = buttons().find((button) => button.props.title === 'Conversations');
  assert.ok(trigger);
  await act(async () => { trigger.props.onClick(); });
  await settle();

  // Hold the listing open so it settles only after the delete has moved the
  // pane onto a new conversation. Both loads an active delete issues (the
  // trailing reload, and the one the sessionId change fires) observe this
  // post-delete server state, in which A no longer exists.
  const pending: Array<Deferred<Array<Record<string, unknown>>>> = [];
  sessionsResponder = () => {
    const deferred = new Deferred<Array<Record<string, unknown>>>();
    pending.push(deferred);
    return deferred.promise;
  };
  const afterDelete = listedSessions.filter((s) => s.session_id !== 'A');

  const deleteA = buttons().find((button) => button.props['aria-label'] === 'Delete conversation: First');
  assert.ok(deleteA);
  await act(async () => { deleteA.props.onClick(); });
  const confirm = buttons().find((button) => nodeText(button) === 'Delete');
  assert.ok(confirm);
  await act(async () => { void confirm.props.onClick(); });
  await settle();

  assert.equal(mounted.context().sessionId, 'C');
  assert.ok(pending.length >= 1, 'the delete must trigger a list refresh');

  await act(async () => {
    for (const deferred of pending) deferred.resolve(afterDelete);
    await Promise.resolve();
  });
  await settle();

  const rendered = buttons().map((button) => nodeText(button)).join('|');
  assert.equal(
    rendered.includes('First'),
    false,
    `the deleted conversation must not return under its old name: ${rendered}`,
  );
  assert.equal(
    rendered.includes('Conversation A'),
    false,
    `the deleted conversation must not be re-synthesized as the active row: ${rendered}`,
  );
  // The surviving conversation and the freshly minted active one are both shown.
  assert.ok(rendered.includes('Second'), `surviving conversation missing: ${rendered}`);
  assert.ok(rendered.includes('Conversation C'), `new active conversation missing: ${rendered}`);
  assert.equal(runtime.deleteCalls.filter((id) => id === 'A').length, 1);

  await unmount(mounted.renderer);
});

test('delete preserves inactive state and moves an active session exactly once', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.mintedIds.push('C');
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  const cHydration = new Deferred<SessionMessagesResponse>();
  runtime.queueMessages('C', () => cHydration.promise);
  storage.setItem('zeroclaw_chat_history_v1:B', '{"messages":[]}');
  storage.setItem('zeroclaw_chat_history_v1:A', '{"messages":[]}');
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  await act(async () => { await mounted.context().removeSession('B'); });
  assert.equal(mounted.context().sessionId, 'A');
  assert.equal(runtime.sockets.length, 1);
  assert.equal(storage.getItem('zeroclaw_chat_history_v1:B'), null);

  await act(async () => { await mounted.context().removeSession('A'); });
  await settle();
  assert.equal(mounted.context().sessionId, 'C');
  assert.equal(mounted.context().hydrated, false);
  assert.equal(mounted.context().sessionPersistence, null);
  assert.equal(storage.getItem('zeroclaw_chat_history_v1:A'), null);
  assert.equal(runtime.sockets[1]?.sessionId, 'C');
  await unmount(mounted.renderer);
});

test('the gateway\'s plain 404 on DELETE surfaces as a status-bearing HttpError', async () => {
  missingSessions.add('gone');
  await assert.rejects(deleteSession('gone'), (err: unknown) => (
    err instanceof HttpError
    && !(err instanceof ApiError)
    && err.status === 404
    && err.message === 'API 404: {"error":"Session not found"}'
  ));
  assert.deepEqual(await deleteSession('present'), { deleted: true, session_id: 'present' });
});

test('deleting an active row the gateway no longer has is treated as deleted', async () => {
  // The picker lists the active conversation even when GET /api/sessions
  // lacks it (another client deleted it, or no turn is stored yet). Deleting
  // that row goes through the real wrapper and hits the real 404 contract.
  const runtime = new FakeSessionRuntime();
  runtime.mintedIds.push('C');
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('C', () => Promise.resolve(messagesResponse('C', true)));
  runtime.queueDelete('A', () => deleteSession('A'));
  missingSessions.add('A');
  storage.setItem('zeroclaw_chat_history_v1:A', '{"messages":[]}');
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  await act(async () => { await mounted.context().removeSession('A'); });
  await settle();
  assert.deepEqual(runtime.deleteCalls, ['A']);
  assert.equal(mounted.context().sessionId, 'C');
  assert.equal(storage.getItem('zeroclaw_chat_history_v1:A'), null);
  assert.equal(runtime.sockets[1]?.sessionId, 'C');
  await unmount(mounted.renderer);
});

test('a non-404 delete failure keeps the active row and its cache', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.mintedIds.push('C');
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueDelete('A', () => deleteSession('A'));
  deleteFailures.add('A');
  storage.setItem('zeroclaw_chat_history_v1:A', '{"messages":[]}');
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  await act(async () => {
    await assert.rejects(mounted.context().removeSession('A'), (err: unknown) => (
      err instanceof HttpError && err.status === 500
    ));
  });
  assert.equal(mounted.context().sessionId, 'A');
  assert.equal(storage.getItem('zeroclaw_chat_history_v1:A'), '{"messages":[]}');
  assert.equal(runtime.sockets.length, 1);
  await unmount(mounted.renderer);
});

test('a listing fetched before an active delete cannot resurrect the deleted row', async () => {
  // The picker refresh started by reopening the menu is still in flight when
  // the operator deletes the active row. The post-delete refresh must not
  // coalesce onto that pre-delete GET and report its rows as the newest.
  const runtime = new FakeSessionRuntime();
  runtime.mintedIds.push('C');
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('C', () => Promise.resolve(messagesResponse('C', true)));
  runtime.queueDelete('A', () => deleteSession('A'));
  const preDelete = new Deferred<Array<Record<string, unknown>>>();
  let listingCalls = 0;
  sessionsResponder = () => {
    listingCalls += 1;
    // 1: the mount-time load, resolved at once so rows A/B are on screen.
    // 2: the refresh started by reopening the picker, held open across the
    //    delete (the pre-delete snapshot). Later: the gateway's post-delete
    //    state.
    if (listingCalls === 1) return Promise.resolve(listedSessions);
    if (listingCalls === 2) return preDelete.promise;
    return Promise.resolve(listedSessions.filter((s) => s.session_id !== 'A'));
  };
  const mounted = await mountChat(runtime, true);
  await openSocket(runtime, 0);
  await settle();
  // Reopening the picker starts the refresh that is still in flight during
  // the delete, while the rows from the mount-time load remain clickable.
  const trigger = mounted.renderer.root.findAllByType('button')
    .find((button) => button.props.title === 'Conversations');
  assert.ok(trigger);
  await act(async () => { trigger.props.onClick(); });
  await settle();

  const deleteButton = mounted.renderer.root.findAllByType('button')
    .find((button) => button.props['aria-label'] === 'Delete conversation: First');
  assert.ok(deleteButton, 'active row A ("First") should expose a delete action');
  await act(async () => { deleteButton.props.onClick(); });
  const confirm = mounted.renderer.root.findAllByType('button')
    .find((button) => nodeText(button) === 'Delete');
  assert.ok(confirm);
  await act(async () => { await confirm.props.onClick(); });
  await settle();
  assert.equal(mounted.context().sessionId, 'C');
  assert.deepEqual(runtime.deleteCalls, ['A']);

  // Now the pre-delete listing lands, carrying the deleted row. The menu is
  // still open from the delete, so no new refresh starts: whatever the rows
  // show here is what the operator sees.
  await act(async () => { preDelete.resolve(listedSessions); });
  await settle();
  const rows = mounted.renderer.root.findAllByType('button')
    .map(nodeText)
    .filter((text) => /^(First|Second|Conversation C)/.test(text));
  assert.equal(rows.some((text) => text.startsWith('First')), false,
    `deleted row resurrected: ${rows.join(' | ')}`);
  assert.ok(rows.some((text) => text.startsWith('Second')));
  assert.ok(rows.some((text) => text.startsWith('Conversation C')));
  assert.ok(listingCalls >= 3, `post-delete refresh must issue a fresh listing request (calls: ${listingCalls})`);
  await unmount(mounted.renderer);
});

test('a late active delete cannot replace a newer selected session', async () => {
  const runtime = new FakeSessionRuntime();
  const deleteA = new Deferred<{ deleted: boolean }>();
  runtime.queueDelete('A', () => deleteA.promise);
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('B', () => Promise.resolve(messagesResponse('B', true, ['B survives'])));
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  const pendingDelete = mounted.context().removeSession('A');
  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  await openSocket(runtime, 1);
  await settle();
  await act(async () => {
    deleteA.resolve({ deleted: true });
    await pendingDelete;
  });
  await settle();

  assert.equal(mounted.context().sessionId, 'B');
  assert.deepEqual(mounted.context().messages.map((message) => message.content), ['B survives']);
  assert.deepEqual(runtime.sockets.map((socket) => socket.sessionId), ['A', 'B']);
  await unmount(mounted.renderer);
});

test('a deferred inactive delete replaces the target if it becomes active', async () => {
  const runtime = new FakeSessionRuntime();
  const deleteB = new Deferred<{ deleted: boolean }>();
  const bHydration = new Deferred<SessionMessagesResponse>();
  const cHydration = new Deferred<SessionMessagesResponse>();
  runtime.mintedIds.push('C');
  runtime.queueDelete('B', () => deleteB.promise);
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('B', () => bHydration.promise);
  runtime.queueMessages('C', () => cHydration.promise);
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  const pendingDelete = mounted.context().removeSession('B');
  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  assert.equal(mounted.context().sessionId, 'B');
  assert.equal(mounted.context().sessionPersistence, null);

  await act(async () => {
    deleteB.resolve({ deleted: true });
    await pendingDelete;
  });
  await settle();

  assert.equal(mounted.context().sessionId, 'C');
  assert.equal(mounted.context().sessionPersistence, null);
  assert.equal(storage.getItem('zeroclaw_active_session.ops'), 'C');
  assert.deepEqual(runtime.sockets.map((socket) => socket.sessionId), ['A', 'B', 'C']);
  await unmount(mounted.renderer);
});

test('a deferred delete replaces its target after an A to B to A round trip', async () => {
  const runtime = new FakeSessionRuntime();
  const deleteA = new Deferred<{ deleted: boolean }>();
  const secondAHydration = new Deferred<SessionMessagesResponse>();
  const cHydration = new Deferred<SessionMessagesResponse>();
  runtime.mintedIds.push('C');
  runtime.queueDelete('A', () => deleteA.promise);
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('B', () => Promise.resolve(messagesResponse('B', true)));
  runtime.queueMessages('A', () => secondAHydration.promise);
  runtime.queueMessages('C', () => cHydration.promise);
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  const pendingDelete = mounted.context().removeSession('A');
  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  assert.equal(mounted.context().sessionPersistence, true);
  assert.equal(await goToSession(mounted, 'A'), true);
  await settle();
  assert.equal(mounted.context().sessionId, 'A');
  assert.equal(mounted.context().sessionPersistence, null);

  await act(async () => {
    deleteA.resolve({ deleted: true });
    await pendingDelete;
  });
  await settle();

  assert.equal(mounted.context().sessionId, 'C');
  assert.equal(mounted.context().sessionPersistence, null);
  assert.equal(storage.getItem('zeroclaw_active_session.ops'), 'C');
  assert.deepEqual(runtime.sockets.map((socket) => socket.sessionId), ['A', 'B', 'A', 'C']);
  await unmount(mounted.renderer);
});

test('composer drafts follow agent and session without crossing conversations', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('B', () => Promise.resolve(messagesResponse('B', true)));
  runtime.queueMessages('B', () => Promise.resolve(messagesResponse('B', true)));
  const mounted = await mountChat(runtime, true);
  await openSocket(runtime, 0);
  await settle();

  await typeInComposer(mounted.renderer, 'draft-A');
  assert.equal(mounted.drafts.get('agent-chat.ops.A'), 'draft-A');

  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  await openSocket(runtime, 1);
  await settle();
  assert.equal(textarea(mounted.renderer).props.value, '');
  await typeInComposer(mounted.renderer, 'draft-B');

  assert.equal(await goToSession(mounted, 'A'), true);
  await settle();
  await openSocket(runtime, 2);
  await settle();
  assert.equal(textarea(mounted.renderer).props.value, 'draft-A');

  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  await openSocket(runtime, 3);
  await settle();
  assert.equal(textarea(mounted.renderer).props.value, 'draft-B');
  await unmount(mounted.renderer);
});

test('a late close from the previous session socket cannot clear the active approval', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('B', () => Promise.resolve(messagesResponse('B', true)));
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  // A parks an approval, then the operator switches to B. The switch resets
  // A's transcript state, so A's approval is already gone before B is current.
  await act(async () => {
    runtime.sockets[0]!.emitMessage({
      type: 'approval_request', request_id: 'req-A', tool: 'shell', timeout_secs: 120,
    });
  });
  assert.equal(mounted.context().pendingApproval?.requestId, 'req-A');
  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  assert.equal(mounted.context().pendingApproval, null);
  assert.equal(runtime.sockets[0]?.disconnectCalls, 1);

  await openSocket(runtime, 1);
  await act(async () => {
    runtime.sockets[1]!.emitMessage({
      type: 'approval_request', request_id: 'req-B', tool: 'shell', timeout_secs: 120,
    });
  });
  assert.equal(mounted.context().pendingApproval?.requestId, 'req-B');

  // The browser delivers A's close asynchronously, after B is current and has
  // its own approval parked. A owns nothing live any more; B's banner stays.
  await act(async () => { runtime.sockets[0]!.emitClose(1000); });
  assert.equal(mounted.context().pendingApproval?.requestId, 'req-B');

  // B's own close still drops B's approval: the gateway auto-denies the parked
  // request when the socket that carried it goes away.
  await act(async () => { runtime.sockets[1]!.emitClose(1006); });
  assert.equal(mounted.context().pendingApproval, null);
  await unmount(mounted.renderer);
});

test('session switch disconnects both the effect-owned and replacement sockets', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  runtime.queueMessages('B', () => Promise.resolve(messagesResponse('B', true)));
  const mounted = await mountChat(runtime);
  await openSocket(runtime, 0);
  await settle();

  await act(async () => { mounted.context().clearAllMessages(); });
  await settle();
  assert.deepEqual(runtime.sockets.map((socket) => socket.sessionId), ['A', 'A']);

  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  assert.ok(runtime.sockets[0]!.disconnectCalls > 0);
  assert.ok(runtime.sockets[1]!.disconnectCalls > 0);
  assert.equal(runtime.sockets[2]?.sessionId, 'B');

  await openSocket(runtime, 2);
  await act(async () => {
    runtime.sockets[1]!.emitOpen();
    runtime.sockets[1]!.emitMessage({ type: 'message', content: 'stale replacement' });
  });
  assert.equal(mounted.context().messages.some((message) =>
    message.content === 'stale replacement'), false);
  await unmount(mounted.renderer);
});

// ── Live composer telemetry and the message-flow group ──────────────────────
//
// This file owns the headless chat integration harness (a real AgentProvider
// over a fake socket, with AgentChatInner mounted), so the composer and
// message-flow wiring is exercised here rather than re-built elsewhere. The
// pure rules live in `lib/sessionStats.logic.test.ts` and
// `lib/messageFlow.logic.test.ts`; these cases pin the seam between them and
// the WebSocket handler, which no pure test can reach.

/** Every frame a gateway turn emits, in the order it emits them. */
function emitLiveTurn(
  socket: FakeSocket,
  options: { input: number; cached: number; output: number; tools: number },
): void {
  socket.emitMessage({ type: 'agent_start', session_id: socket.sessionId });
  socket.emitMessage({ type: 'thinking', content: 'let me look' });
  socket.emitMessage({ type: 'chunk', content: 'checking the file' });
  // `usage` closes the step, so the tool calls below attach to it.
  socket.emitMessage({
    type: 'usage',
    input_tokens: options.input,
    cached_input_tokens: options.cached,
    output_tokens: options.output,
  });
  for (let i = 0; i < options.tools; i++) {
    socket.emitMessage({ type: 'tool_call', id: `call_${i}`, name: 'shell', args: { i } });
    socket.emitMessage({ type: 'tool_result', id: `call_${i}`, name: 'shell', output: `out ${i}` });
  }
  socket.emitMessage({ type: 'chunk', content: 'the answer' });
  socket.emitMessage({ type: 'usage', input_tokens: options.input, output_tokens: options.output });
  socket.emitMessage({
    type: 'done',
    full_response: 'the answer',
    input_tokens: options.input * 2,
    output_tokens: options.output * 2,
    last_input_tokens: options.input,
    max_context_tokens: 1000,
    steps: 2,
    cached_input_tokens: options.cached,
  });
}

/**
 * Mount a chat with one open socket and a hydrated empty conversation.
 *
 * `toolActivity` stands in for the toolbar's Wrench toggle. The component reads
 * it from `localStorage` once, in a `useState` initializer, so the key is
 * written before the mount and cleared right after — otherwise the choice would
 * leak into the next case in this file.
 */
async function mountLiveChat(
  options: { toolActivity?: boolean } = {},
): Promise<{
  runtime: FakeSessionRuntime;
  mounted: MountedChat;
  socket: FakeSocket;
}> {
  const runtime = new FakeSessionRuntime();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true)));
  if (options.toolActivity) storage.setItem('zeroclaw_show_tool_activity', '1');
  const mounted = await mountChat(runtime, true);
  storage.removeItem('zeroclaw_show_tool_activity');
  await openSocket(runtime, 0);
  await settle();
  return { runtime, mounted, socket: runtime.sockets[0]! };
}

function renderedText(mounted: MountedChat): string {
  return nodeText(mounted.renderer.root);
}

/** The collapsed tool-group headers on screen — one per folded turn.
 *  Read as buttons, not as text: `nodeText` flattens the tree, so the count is
 *  the only thing that tells a folded turn from loose cards. */
function groupHeaders(mounted: MountedChat): ReactTestInstance[] {
  return mounted.renderer.root
    .findAllByType('button')
    .filter((button) => /tool calls · \d+ messages/.test(nodeText(button)));
}

function expandGroupButton(mounted: MountedChat): ReactTestInstance | undefined {
  return mounted.renderer.root
    .findAllByType('button')
    .find((button) => button.props['aria-label'] === 'Expand the tool-call trajectory');
}

test('the stats row recounts the message list and accumulates tokens live', async () => {
  const { mounted, socket } = await mountLiveChat();
  assert.equal(mounted.context().liveStats.turns, 0);
  assert.equal(mounted.context().liveStats.steps, 0);
  assert.match(renderedText(mounted), /0 turns 0 steps/);

  // Tokens move on `usage` during the turn; turns/steps wait for bubbles in
  // the message list (user send + committed answer).
  await act(async () => {
    mounted.context().sendMessage('analyze this');
  });
  assert.equal(mounted.context().liveStats.turns, 1);
  assert.equal(mounted.context().liveStats.steps, 0);

  await act(async () => {
    socket.emitMessage({ type: 'agent_start', session_id: socket.sessionId });
    socket.emitMessage({
      type: 'usage',
      input_tokens: 17214,
      output_tokens: 32,
      cached_input_tokens: 1536,
    });
  });
  assert.equal(mounted.context().liveStats.turns, 1);
  assert.equal(mounted.context().liveStats.steps, 0);
  assert.match(renderedText(mounted), /17\.2K tok/);
  assert.match(renderedText(mounted), /cache hit 9%/);

  await act(async () => {
    socket.emitMessage({
      type: 'done',
      full_response: 'the answer',
      last_input_tokens: 17214,
      max_context_tokens: 1000,
      steps: 1,
      cached_input_tokens: 1536,
    });
  });
  const stats = mounted.context().liveStats;
  assert.deepEqual(
    {
      turns: stats.turns,
      steps: stats.steps,
      input: stats.input,
      output: stats.output,
      cached: stats.cached,
    },
    { turns: 1, steps: 1, input: 17214, output: 32, cached: 1536 },
  );
  assert.match(renderedText(mounted), /1 turns 1 steps/);
  await unmount(mounted.renderer);
});

test('a turn is built from frames alone: it adds no session request', async () => {
  // Composer acceptance 5, restated as message-flow acceptance 8. The row and
  // the trajectory are both derived from WebSocket frames the client already
  // receives, so the whole feature has to cost zero requests — measured here
  // rather than asserted by inspection.
  const { mounted, socket } = await mountLiveChat();

  // Proves the counter is wired: without this, a broken counter would let the
  // assertion below pass for the wrong reason.
  assert.equal(
    sessionRequests.length > 0,
    true,
    'opening a conversation lists sessions, as it does today',
  );

  sessionRequests = [];
  await act(async () => {
    mounted.context().sendMessage('analyze this');
  });
  await act(async () => {
    emitLiveTurn(socket, { input: 100, cached: 80, output: 10, tools: 3 });
  });
  await settle();

  assert.deepEqual(
    sessionRequests,
    [],
    'a full turn must not touch the REST surface',
  );
  await unmount(mounted.renderer);
});

test('a step with no tool calls stays its own step', async () => {
  // The step boundary is the `usage` frame (message-flow §8.5). If the view
  // instead waited for the next `tool_call` to close a step — the fallback the
  // plan calls unreliable — this turn would collapse into one step: the
  // intermediate reply would be welded onto the answer, and the turn would have
  // no trajectory to group at all.
  const { mounted, socket } = await mountLiveChat();
  await act(async () => {
    mounted.context().sendMessage('summarize');
  });
  await act(async () => {
    socket.emitMessage({ type: 'agent_start', session_id: socket.sessionId });
    socket.emitMessage({ type: 'chunk', content: 'let me think' });
    socket.emitMessage({ type: 'usage', input_tokens: 10, output_tokens: 2 });
    socket.emitMessage({ type: 'chunk', content: 'the answer' });
    socket.emitMessage({ type: 'usage', input_tokens: 20, output_tokens: 3 });
    socket.emitMessage({
      type: 'done',
      full_response: 'the answer',
      last_input_tokens: 20,
      max_context_tokens: 1000,
      steps: 2,
    });
  });
  await settle();

  const committed = mounted.context().messages.find((message) => message.segments);
  assert.ok(committed, 'two steps is a trajectory worth rendering');
  assert.equal(committed.segments!.steps.length, 1, 'the intermediate step survives');
  assert.equal(committed.segments!.steps[0]!.text, 'let me think');
  assert.equal(committed.content, 'the answer');
  await unmount(mounted.renderer);
});

test('the trajectory renders while the turn streams, expanded, and collapses on commit', async () => {
  // message-flow §5.4 / D1: the streaming view *is* the trajectory group, not a
  // flat bubble beside it. Expanded while the turn runs, collapsed once the
  // committed message takes over.
  const { mounted, socket } = await mountLiveChat({ toolActivity: true });
  await act(async () => {
    mounted.context().sendMessage('analyze this');
  });
  await act(async () => {
    socket.emitMessage({ type: 'agent_start', session_id: socket.sessionId });
    socket.emitMessage({ type: 'chunk', content: 'checking the file' });
    socket.emitMessage({ type: 'usage', input_tokens: 100, output_tokens: 10 });
    socket.emitMessage({ type: 'tool_call', id: 'call_1', name: 'shell', args: { i: 1 } });
  });
  await settle();

  const streaming = renderedText(mounted);
  assert.match(streaming, /1 tool calls · 1 messages/, `the live group has a header: ${streaming}`);
  // Expanded, so the body is in the tree at all — `ToolCallGroup` renders it
  // conditionally rather than hiding it with CSS.
  assert.match(streaming, /checking the file/, `the step is open: ${streaming}`);
  assert.equal(
    (streaming.match(/shell/g) ?? []).length,
    1,
    `the call renders once, not once in the group and once as a loose card: ${streaming}`,
  );

  await act(async () => {
    socket.emitMessage({ type: 'tool_result', id: 'call_1', name: 'shell', output: 'out 1' });
    socket.emitMessage({ type: 'chunk', content: 'the answer' });
    socket.emitMessage({ type: 'usage', input_tokens: 200, output_tokens: 20 });
    socket.emitMessage({
      type: 'done',
      full_response: 'the answer',
      last_input_tokens: 200,
      max_context_tokens: 1000,
      steps: 2,
    });
  });
  await settle();

  const committed = renderedText(mounted);
  assert.match(committed, /1 tool calls · 1 messages/, 'the header survives the commit');
  assert.equal(committed.includes('checking the file'), false, 'collapsed on commit');
  assert.match(committed, /the answer/);
  await unmount(mounted.renderer);
});

test('a tool-free turn streams without ever growing a group', async () => {
  // The answer-in-progress sits below the group, so the commonest turn of all
  // must not flash a `0 次工具调用` header on its way through.
  const { mounted, socket } = await mountLiveChat({ toolActivity: true });
  await act(async () => {
    mounted.context().sendMessage('hi');
  });
  await act(async () => {
    socket.emitMessage({ type: 'agent_start', session_id: socket.sessionId });
    socket.emitMessage({ type: 'chunk', content: 'hello' });
  });
  await settle();
  assert.match(renderedText(mounted), /hello/, 'the text streams in the bubble');
  assert.equal(/tool calls/.test(renderedText(mounted)), false, 'and no empty group');
  await unmount(mounted.renderer);
});

test('the composer carries the ring, and the linear context bar is gone', async () => {
  // P4 check 1. The ring is the bar's replacement, so "one is present" and
  // "the other is absent" are two readings of the same fact.
  const { mounted, socket } = await mountLiveChat();
  const findRing = () => mounted.renderer.root
    .findAllByType('button')
    .find((button) => button.props['aria-label'] === 'Context usage')!;

  assert.equal(findRing().findAllByType('circle').length, 1, 'track only');

  await act(async () => {
    findRing().props.onClick();
  });
  assert.equal(findRing().props['aria-expanded'], true, 'opens on click');
  const empty = renderedText(mounted);
  assert.match(empty, /used 0/);
  assert.match(empty, /limit —/, 'an unreported window says so instead of guessing');
  assert.equal(/% used/.test(empty), false, 'and no percentage without a window');

  // Let the gateway report a window: the ring fills and the panel gains the
  // percentage it withheld. The values are the `done` frame's, which the ring
  // shares with the rest of the context meter rather than fetching its own.
  await act(async () => {
    socket.emitMessage({
      type: 'done',
      full_response: 'hi',
      last_input_tokens: 250,
      max_context_tokens: 1000,
      steps: 1,
    });
  });
  const filled = renderedText(mounted);
  assert.equal(findRing().findAllByType('circle').length, 2, 'track plus fill');
  assert.match(filled, /used 250/);
  assert.match(filled, /limit 1,000/);
  assert.match(filled, /25% used/);
  await unmount(mounted.renderer);
});

test('the row never renders a dollar figure or NaN', async () => {
  const { mounted, socket } = await mountLiveChat();
  await act(async () => {
    mounted.context().sendMessage('hi');
  });
  await act(async () => {
    socket.emitMessage({ type: 'agent_start', session_id: socket.sessionId });
    // A step whose provider reported nothing at all.
    socket.emitMessage({ type: 'usage' });
    socket.emitMessage({ type: 'done', full_response: 'hi', steps: 1 });
  });
  const text = renderedText(mounted);
  assert.match(text, /1 turns 1 steps/);
  assert.equal(text.includes('$'), false, 'production models are unpriced — no cost segment');
  assert.equal(/NaN|Infinity/.test(text), false);
  assert.match(text, /cache hit 0%/, 'no input reported still shows 0%, not a hidden segment');
  await unmount(mounted.renderer);
});

test('a cancelled turn still keeps tokens; turns/steps follow the list', async () => {
  const { mounted, socket } = await mountLiveChat();
  await act(async () => {
    mounted.context().sendMessage('cancel me');
  });
  await act(async () => {
    socket.emitMessage({ type: 'agent_start', session_id: socket.sessionId });
    socket.emitMessage({
      type: 'usage',
      input_tokens: 120,
      output_tokens: 7,
      cached_input_tokens: 64,
    });
    socket.emitMessage({
      type: 'aborted',
      input_tokens: 120,
      output_tokens: 7,
      cached_input_tokens: 64,
      steps: 1,
    });
  });
  const stats = mounted.context().liveStats;
  // No committed assistant bubble on abort → steps stay 0; user bubble remains.
  assert.equal(stats.turns, 1);
  assert.equal(stats.steps, 0);
  assert.equal(stats.input, 120);
  assert.equal(stats.cached, 64);
  await unmount(mounted.renderer);
});

test('a trim resets tokens; turns/steps follow the cut list', async () => {
  const { runtime, mounted, socket } = await mountLiveChat();
  await act(async () => {
    mounted.context().sendMessage('keep me');
  });
  await act(async () => {
    socket.emitMessage({ type: 'agent_start', session_id: socket.sessionId });
    socket.emitMessage({ type: 'usage', input_tokens: 100, output_tokens: 5 });
    socket.emitMessage({ type: 'done', full_response: 'ok', steps: 1 });
  });
  assert.equal(mounted.context().liveStats.turns, 1);
  assert.equal(mounted.context().liveStats.steps, 1);
  assert.equal(mounted.context().liveStats.input, 100);

  // Trim clears live token totals and cuts the list locally by user-turn
  // boundary. No response is queued on purpose: a frame that re-read the store
  // would land on the empty default and drop the kept answer with it, so the
  // turn/step counts below are what tell the two implementations apart.
  await act(async () => {
    socket.emitMessage({
      type: 'history_trimmed',
      dropped_messages: 4,
      kept_turns: 1,
      reason: 'budget',
    });
  });
  await settle();
  assert.equal(mounted.context().liveStats.input, 0);
  assert.equal(mounted.context().liveStats.turns, 1, 'kept user bubble still counts');
  assert.equal(mounted.context().liveStats.steps, 1, 'and the kept turn keeps its answer');
  assert.match(renderedText(mounted), /1 turns 1 steps/);

  runtime.queueMessages('B', () => Promise.resolve(messagesResponse('B', true)));
  assert.equal(await goToSession(mounted, 'B'), true);
  await settle();
  assert.equal(mounted.context().liveStats.turns, 0);
  assert.equal(mounted.context().liveStats.steps, 0);
  await unmount(mounted.renderer);
});

test('a trim cuts the transcript locally, so committed turns stay folded', async () => {
  const { runtime, mounted, socket } = await mountLiveChat({ toolActivity: true });

  // Two turns watched live: each commits with a trajectory, so each folds.
  for (const prompt of ['first turn', 'second turn']) {
    await act(async () => { mounted.context().sendMessage(prompt); });
    await act(async () => {
      emitLiveTurn(socket, { input: 100, cached: 80, output: 10, tools: 2 });
    });
    await settle();
  }
  assert.equal(groupHeaders(mounted).length, 2, 'both committed turns fold');

  // The trap: this response carries no tool data at all. A frame that rebuilt
  // the list from the store would therefore strip the surviving turn's
  // trajectory and turn its folded group back into loose cards.
  const callsBefore = runtime.messageCalls.length;
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true, ['the answer'])));

  await act(async () => {
    socket.emitMessage({
      type: 'history_trimmed',
      dropped_messages: 4,
      kept_turns: 1,
      reason: 'budget',
      tokens_after: 4200,
    });
  });
  await settle();

  assert.equal(
    runtime.messageCalls.length,
    callsBefore,
    'the frame cuts locally — it must not re-read the store',
  );
  assert.equal(mounted.context().contextInputTokens, 4200, 'tokens_after still lands');
  assert.equal(
    groupHeaders(mounted).length,
    1,
    'the kept turn is still folded; the dropped one took its group with it',
  );

  const text = renderedText(mounted);
  assert.equal(text.includes('first turn'), false, 'the dropped turn is off the screen');
  assert.match(text, /2 tool calls · 1 messages/, 'and the kept turn kept its own header');
  assert.equal(text.includes('out 0'), false, 'still collapsed, so its body is not rendered');
  assert.match(text, /Earlier conversation history was trimmed/);

  // Expanding proves the group holds the live trajectory, not a rebuilt shadow.
  const expand = expandGroupButton(mounted);
  assert.ok(expand, 'the folded group still offers to expand');
  await act(async () => { expand!.props.onClick(); });
  const expanded = renderedText(mounted);
  for (const piece of ['let me look', 'checking the file', 'out 0', 'out 1']) {
    assert.ok(expanded.includes(piece), `expanded group holds ${piece}`);
  }
  await unmount(mounted.renderer);
});

test('a live turn renders one group holding its trajectory, answer outside it', async () => {
  const { mounted, socket } = await mountLiveChat({ toolActivity: true });
  await act(async () => {
    mounted.context().sendMessage('analyze this');
  });
  await act(async () => {
    emitLiveTurn(socket, { input: 100, cached: 80, output: 10, tools: 3 });
  });
  await settle();

  const text = renderedText(mounted);
  // One step made all three calls, and that step carries the turn's thinking
  // and text — so the group holds one message. The final step is the answer, so
  // it sits outside the group and is not counted.
  assert.match(text, /3 tool calls · 1 messages/, 'one collapsed group for the whole turn');
  assert.match(text, /the answer/, 'the final answer renders below the group');

  // The group carries the trajectory, so the turn's calls appear once, not
  // twice: the loose cards are absorbed by it.
  const committed = mounted.context().messages.find((message) => message.segments);
  assert.ok(committed, 'the committed turn carries its trajectory');
  assert.equal(committed.segments!.steps.length, 1, 'one step made all three calls');
  assert.equal(committed.segments!.steps[0]!.toolCalls.length, 3);
  assert.deepEqual(
    committed.segments!.steps[0]!.toolCalls.map((call) => call.output),
    ['out 0', 'out 1', 'out 2'],
  );
  assert.equal(committed.content, 'the answer');

  // P4 check 3's ordering claim: expanding shows each step's thinking, then its
  // text, then its cards — the order the frames arrived in.
  const header = mounted.renderer.root
    .findAllByType('button')
    .find((button) => button.props['aria-label'] === 'Expand the tool-call trajectory');
  assert.ok(header, 'the collapsed group offers to expand');
  await act(async () => {
    header!.props.onClick();
  });
  const expanded = renderedText(mounted);
  const order = ['let me look', 'checking the file', 'out 0', 'out 1', 'out 2', 'the answer']
    .map((needle) => expanded.indexOf(needle));
  assert.equal(order.includes(-1), false, `every piece renders once expanded: ${expanded}`);
  assert.deepEqual(order, [...order].sort((a, b) => a - b), 'thinking, text and cards in order');
  await unmount(mounted.renderer);
});

test('with tool activity hidden the group goes with it, and the answer stays', async () => {
  // The toolbar toggle is the product's existing call — "tool execution is
  // plumbing, not chat" — and the group *is* tool activity: its header counts
  // calls and its body holds the cards. So it follows the toggle. What must
  // survive is the answer, which is the turn's record.
  const { mounted, socket } = await mountLiveChat();
  await act(async () => {
    mounted.context().sendMessage('analyze this');
  });
  await act(async () => {
    emitLiveTurn(socket, { input: 100, cached: 80, output: 10, tools: 3 });
  });
  await settle();

  const text = renderedText(mounted);
  assert.equal(/tool calls/.test(text), false, 'no header while tool activity is off');
  assert.equal(text.includes('out 0'), false, 'and no loose cards standing in for it');
  assert.match(text, /the answer/);
  // The trajectory is still captured — the toggle is a render-time filter, so
  // turning it on reveals this turn's steps retroactively.
  assert.ok(mounted.context().messages.some((message) => message.segments));
  await unmount(mounted.renderer);
});

test('a turn with no tool calls renders as a plain bubble, with no empty group', async () => {
  const { mounted, socket } = await mountLiveChat();
  await act(async () => {
    mounted.context().sendMessage('hi');
  });
  await act(async () => {
    socket.emitMessage({ type: 'agent_start', session_id: socket.sessionId });
    socket.emitMessage({ type: 'chunk', content: 'hello' });
    socket.emitMessage({ type: 'usage', input_tokens: 10, output_tokens: 2 });
    socket.emitMessage({ type: 'done', full_response: 'hello', steps: 1 });
  });
  await settle();
  assert.equal(/tool calls/.test(renderedText(mounted)), false);
  assert.equal(mounted.context().messages.some((message) => message.segments), false);
  await unmount(mounted.renderer);
});

test('a hydrated conversation renders ungrouped: the trajectory is live-only', async () => {
  const runtime = new FakeSessionRuntime();
  runtime.queueMessages('A', () => Promise.resolve(messagesResponse('A', true, ['earlier prompt'])));
  const mounted = await mountChat(runtime, true);
  await openSocket(runtime, 0);
  await settle();

  // Hydrated list: one user prompt, no assistant yet → 1 turn 0 steps.
  assert.equal(mounted.context().liveStats.turns, 1);
  assert.equal(mounted.context().liveStats.steps, 0);
  assert.match(renderedText(mounted), /1 turns 0 steps/);

  await act(async () => {
    runtime.sockets[0]!.emitMessage({
      type: 'message',
      content: '{"content":null,"tool_calls":[{"id":"c1","name":"shell","arguments":"{}"}]}',
    });
  });
  await settle();
  assert.equal(/tool calls/.test(renderedText(mounted)), false);
  assert.equal(mounted.context().messages.some((message) => message.segments), false);
  // The `message` frame still commits an agent bubble (no segments) → +1 step.
  assert.equal(mounted.context().liveStats.steps, 1);
  await unmount(mounted.renderer);
});
