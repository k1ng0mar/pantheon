// The phone's Connect client: a viewer of tasks on the user's computers (it never hosts).
// Owns the relay socket, the pending calls and the store; screens call `connect.*` and read
// state with the hooks in src/store. See README.md beside this file.
import './polyfill.ts';
import { initialState, setPublicKey, taskKey, useConnect, type ConnectState } from '../store/index.ts';
import { applyEvent, fromSubscribe, idleLive, lastId, mergeMessages } from '../store/reduce.ts';
import * as auth from './auth.ts';
import { build, NOT_CONFIGURED } from './config.ts';
import { open, pairKey, publicKey, seal } from './crypto.ts';
import { LOST, OFFLINE, Pending, RATE, request, START_TIMEOUT_MS, TIMEOUT_MS, type Method } from './rpc.ts';
import * as secrets from './secrets.ts';
import { RelaySocket } from './socket.ts';
import { isTrusted, loadPins, prunePins, setTrusted as pin } from './trust.ts';
import type { AgentEvent, ConfigureParams, Device, MessagePage, Provider, ProviderModel, RunFinished, StartParams, SubscribedTask, TasksList } from './types.ts';

const RENEW_MS = 9 * 60_000;
const REFRESH_MS = 12 * 3600_000;
const MAX_FRAME = 256 * 1024;
const MAX_PAGES = 50;

let token: string | null = null;
let secret: Uint8Array | null = null;
let presence = false;
let signInAbort: AbortController | null = null;
let timers: ReturnType<typeof setInterval>[] = [];
const pending = new Pending();
/** Pair keys by device id, with the public key they were derived from. */
const keys = new Map<string, { publicKey: string; key: Uint8Array }>();
/** Tasks the UI has open, per host; subscribed, renewed, and re-subscribed on presence. */
const watched = new Map<string, Set<string>>();
const loads = new Map<string, Promise<void>>();

const state = () => useConnect.getState();
const patch = (update: Partial<ConnectState> | ((s: ConnectState) => Partial<ConnectState>)) => useConnect.setState(update);

const socket = new RelaySocket({
  token: () => token,
  url: () => `${build!.relay.replace(/^https:/, 'wss:')}/v1/connect?host=0`,
  onOpen: () => {
    patch({ connected: true, error: null });
    void refreshIfDue();
    timers = [setInterval(() => void subscribeAll(), RENEW_MS), setInterval(() => void refreshIfDue(), REFRESH_MS)];
  },
  onClose: () => {
    timers.forEach(clearInterval);
    timers = [];
    pending.failAll(LOST);
    patch({ connected: false });
  },
  onText: frame,
  onRefused: (used, revoked) => {
    if (revoked || auth.expired(used)) return signOutIf(used);
    // A WebSocket upgrade hides its HTTP status; ask the API whether the token still works.
    auth.listDevices(used).catch(error => { if (error instanceof auth.SignedOut) signOutIf(error.token); });
  },
});

/** Reads secrets and connects when signed in. Call once at app start. */
async function init(): Promise<void> {
  if (state().ready) return;
  const [saved, account, deviceId, pins] = await Promise.all([secrets.getToken(), secrets.getAccount(), secrets.getDeviceId(), loadPins()]);
  secret = await secrets.getSecret();
  setPublicKey(publicKey(secret));
  token = saved && !auth.expired(saved) && build ? saved : null;
  patch({ ready: true, deviceId, pins, signedIn: !!token, account: token ? account : null });
  if (token) socket.start();
}

async function signInStart(): Promise<auth.SignInCode> {
  if (!build) throw new Error(NOT_CONFIGURED);
  signInAbort?.abort();
  signInAbort = new AbortController();
  return auth.startSignIn();
}

/** Waits for approval on GitHub and registers this phone. */
async function signInFinish(code: auth.SignInCode): Promise<void> {
  const abort = signInAbort ?? new AbortController();
  const result = await auth.finishSignIn(code, abort.signal);
  if (signInAbort === abort) signInAbort = null;
  token = result.token;
  presence = false;
  patch({ signedIn: true, account: result.account, deviceId: result.deviceId, error: null });
  socket.stop();
  socket.start();
}

function signInCancel() {
  signInAbort?.abort();
  signInAbort = null;
}

async function signOut(): Promise<void> {
  socket.stop();
  pending.failAll('Signed out.');
  token = null;
  keys.clear();
  watched.clear();
  presence = false;
  await secrets.clearSession();
  const { deviceId, fingerprint, pins, ready } = state();
  useConnect.setState({ ...initialState(), deviceId, fingerprint, pins, ready }, true);
}

/** A refusal signs out only if it refused the token that is still current. */
function signOutIf(used: string) {
  if (token !== used) return;
  void signOut().then(() => patch({ error: auth.SIGNED_OUT }));
}

async function refreshIfDue() {
  const old = token;
  if (!old || !auth.needsRefresh(old)) return;
  try {
    const fresh = await auth.refreshToken(old);
    if (!fresh || token !== old) return;
    token = fresh;
    await secrets.setToken(fresh);
  } catch (error) {
    if (error instanceof auth.SignedOut) signOutIf(error.token);
  }
}

// ---- Devices and trust ----

const validDevice = (d: any): d is Device =>
  d && typeof d.id === 'string' && /^[\x21-\x7e]{1,200}$/.test(d.id) && typeof d.name === 'string' && typeof d.publicKey === 'string';

async function applyDevices(list: unknown[]) {
  const devices = list.filter(validDevice).slice(0, 64);
  for (const id of keys.keys()) if (!devices.some(d => d.id === id && d.publicKey === keys.get(id)!.publicKey)) keys.delete(id);
  for (const id of watched.keys()) if (!devices.some(d => d.id === id)) watched.delete(id);
  patch({ devices, pins: await prunePins(state().pins, devices) });
}

/** The device list over HTTP; only needed before the socket's first presence frame. */
async function refreshDevices(): Promise<void> {
  if (!token || presence) return;
  const used = token;
  try {
    const devices = await auth.listDevices(used);
    if (!presence) await applyDevices(devices);
  } catch (error) {
    if (error instanceof auth.SignedOut) signOutIf(error.token);
    throw error;
  }
}

async function setTrusted(deviceId: string, trusted: boolean): Promise<void> {
  const device = state().devices.find(d => d.id === deviceId);
  if (!device) throw new Error('That device is no longer on your account.');
  patch({ pins: await pin(state().pins, device, trusted) });
  if (trusted) void subscribeHost(deviceId);
}

async function revokeDevice(deviceId: string): Promise<void> {
  if (!token) throw new Error('Sign in to PandaCode Connect first.');
  const used = token;
  await auth.revokeDevice(used, deviceId).catch(error => {
    if (error instanceof auth.SignedOut) signOutIf(error.token);
    throw error;
  });
  if (deviceId === state().deviceId) return signOut();
}

// ---- Calls ----

function callable(deviceId: string): Device {
  if (!token) throw new Error('Sign in to PandaCode Connect first.');
  const s = state();
  if (deviceId === s.deviceId) throw new Error('That is this phone.');
  const device = s.devices.find(d => d.id === deviceId);
  if (!device) throw new Error('That computer is no longer on your account.');
  if (!isTrusted(s.pins, device)) throw new Error(`${device.name} isn't trusted on this phone yet. Trust it in Settings.`);
  if (!device.online) throw new Error(OFFLINE);
  return device;
}

function pairFor(device: Device): Uint8Array {
  const cached = keys.get(device.id);
  if (cached?.publicKey === device.publicKey) return cached.key;
  const key = pairKey(secret!, state().deviceId, device.publicKey, device.id);
  keys.set(device.id, { publicKey: device.publicKey, key });
  return key;
}

async function call<T>(deviceId: string, method: Method, params: unknown, timeoutMs = TIMEOUT_MS): Promise<T> {
  const device = callable(deviceId);
  const message = request(method, params);
  const body = seal(pairFor(device), state().deviceId, device.id, JSON.stringify(message));
  const frame = JSON.stringify({ to: device.id, body, ref: message.id });
  if (frame.length > MAX_FRAME) throw new Error('That request is too large.');
  const reply = pending.wait(device.id, message.id, timeoutMs);
  if (!socket.send(frame)) pending.fail(device.id, message.id, 'Not connected to the relay.');
  return reply as Promise<T>;
}

// ---- Incoming frames ----

function frame(text: string) {
  let value: any;
  try { value = JSON.parse(text); } catch { return; }
  switch (value?.kind) {
    case 'presence':
      if (!Array.isArray(value.devices)) return;
      presence = true;
      void applyDevices(value.devices).then(subscribeAll);
      return;
    case 'undeliverable':
      if (typeof value.to === 'string') pending.fail(value.to, typeof value.ref === 'string' ? value.ref : undefined, value.reason === 'rate' ? RATE : OFFLINE);
      return;
    case 'error':
      patch({ error: typeof value.error === 'string' ? `Relay: ${value.error.slice(0, 300)}` : 'The relay reported an error.' });
      return;
  }
  const { from, to, body } = value ?? {};
  const s = state();
  if (typeof from !== 'string' || typeof body !== 'string' || to !== s.deviceId || from === s.deviceId) return;
  const device = s.devices.find(d => d.id === from);
  // Only a device trusted here, with the key presence lists now, may answer or send events.
  if (!device || !isTrusted(s.pins, device)) return;
  let message: any;
  try { message = JSON.parse(open(pairFor(device), from, to, body)); } catch { return; }
  if (message?.type === 'res') pending.resolve(from, message);
  else if (message?.type === 'event' && message.name === 'agent.events') events(from, message.data?.events);
  else if (message?.type === 'event' && message.name === 'run.finished') void finished(from, message.data);
}

function events(host: string, list: unknown) {
  const ids = watched.get(host);
  if (!ids || !Array.isArray(list)) return;
  patch(s => {
    const live = { ...s.live };
    for (const event of list as AgentEvent[]) {
      if (typeof event?.sessionId !== 'string' || typeof event.kind !== 'string' || !ids.has(event.sessionId)) continue;
      const key = taskKey(host, event.sessionId);
      live[key] = applyEvent(live[key] ?? idleLive(), { ...event, text: typeof event.text === 'string' ? event.text : '' });
    }
    return { live, tasks: withStatuses(s, host, live) };
  });
}

/** Mirrors live statuses into the host's task list. */
function withStatuses(s: ConnectState, host: string, live: ConnectState['live']): ConnectState['tasks'] {
  const list = s.tasks[host];
  if (!list) return s.tasks;
  let changed = false;
  const tasks = list.tasks.map(t => {
    const status = live[taskKey(host, t.sessionId)]?.status;
    if (!status || status === t.status) return t;
    changed = true;
    return { ...t, status };
  });
  // Unchanged → same object, so a streamed delta doesn't re-render task lists.
  return changed ? { ...s.tasks, [host]: { ...list, tasks } } : s.tasks;
}

async function finished(host: string, data: RunFinished) {
  if (typeof data?.sessionId !== 'string' || !watched.get(host)?.has(data.sessionId)) return;
  await loadMessages(host, data.sessionId).catch(() => undefined); // the next subscribe retries
  const key = taskKey(host, data.sessionId);
  const status = data.stopped ? 'stopped' : data.successful ? 'idle' : 'failed';
  patch(s => {
    const live = { ...s.live, [key]: { ...idleLive(), status } };
    return { live, tasks: withStatuses(s, host, live) };
  });
}

// ---- Subscriptions and messages ----

/** Keeps these tasks' events and messages flowing while the UI shows them. */
function watch(deviceId: string, sessionIds: string[]) {
  const ids = watched.get(deviceId) ?? new Set();
  const before = ids.size;
  sessionIds.forEach(id => ids.add(id));
  watched.set(deviceId, ids);
  if (ids.size !== before) void subscribeHost(deviceId);
}

function unwatch(deviceId: string, sessionIds: string[]) {
  const ids = watched.get(deviceId);
  sessionIds.forEach(id => ids?.delete(id));
  if (ids && !ids.size) watched.delete(deviceId);
}

async function subscribeAll() {
  await Promise.all([...watched.keys()].map(subscribeHost));
}

/** Subscribe replies catch up on missed status changes, pending requests and messages. */
async function subscribeHost(deviceId: string): Promise<void> {
  const ids = [...(watched.get(deviceId) ?? [])].slice(0, 500);
  if (!ids.length || !socket.open) return;
  let reply: { tasks: SubscribedTask[] };
  try {
    reply = await call(deviceId, 'subscribe', { sessionIds: ids });
  } catch {
    return; // offline or untrusted; the next presence frame or renewal retries
  }
  const tasks = Array.isArray(reply?.tasks) ? reply.tasks : [];
  patch(s => {
    const live = { ...s.live };
    for (const t of tasks) live[taskKey(deviceId, t.sessionId)] = fromSubscribe(live[taskKey(deviceId, t.sessionId)] ?? idleLive(), t.status, t.pending);
    return { live, tasks: withStatuses(s, deviceId, live) };
  });
  for (const t of tasks) if (t.lastId > lastId(state().messages[taskKey(deviceId, t.sessionId)])) void loadMessages(deviceId, t.sessionId).catch(() => undefined);
}

/** Copies messages newer than the last stored one, following hasMore. Calls run one at a time per task. */
function loadMessages(deviceId: string, sessionId: string): Promise<void> {
  const key = taskKey(deviceId, sessionId);
  const run = async () => {
    for (let page = 0; page < MAX_PAGES; page++) {
      const afterId = lastId(state().messages[key]);
      const reply = await call<MessagePage>(deviceId, 'task.messages', { sessionId, afterId });
      const messages = Array.isArray(reply?.messages) ? reply.messages : [];
      patch(s => ({ messages: { ...s.messages, [key]: mergeMessages(s.messages[key] ?? [], messages) }, tasks: withTitle(s, deviceId, sessionId, reply?.title) }));
      if (!reply?.hasMore || !messages.length) return;
    }
  };
  const next = (loads.get(key) ?? Promise.resolve()).catch(() => undefined).then(run);
  loads.set(key, next);
  return next.finally(() => { if (loads.get(key) === next) loads.delete(key); });
}

function withTitle(s: ConnectState, host: string, sessionId: string, title: unknown): ConnectState['tasks'] {
  const list = s.tasks[host];
  if (!list || typeof title !== 'string' || !title) return s.tasks;
  return { ...s.tasks, [host]: { ...list, tasks: list.tasks.map(t => (t.sessionId === sessionId && t.title !== title ? { ...t, title } : t)) } };
}

// ---- Typed methods ----

async function listTasks(deviceId: string, limit?: number): Promise<TasksList> {
  const reply = await call<TasksList>(deviceId, 'tasks.list', limit === undefined ? {} : { limit });
  const list = { tasks: Array.isArray(reply?.tasks) ? reply.tasks : [], projects: Array.isArray(reply?.projects) ? reply.projects : [] };
  patch(s => {
    // A task already waiting can be answered as soon as it opens: seed its requests from the list.
    const live = { ...s.live };
    for (const t of list.tasks) {
      const key = taskKey(deviceId, t.sessionId);
      if (Array.isArray(t.pending) && t.pending.length && !live[key]) live[key] = fromSubscribe(idleLive(), t.status, t.pending);
    }
    return { live, tasks: { ...s.tasks, [deviceId]: { ...list, loadedAt: Date.now() } } };
  });
  return list;
}

async function listProviders(deviceId: string, force = false): Promise<Provider[]> {
  const providers = await call<Provider[]>(deviceId, 'providers.list', { force });
  patch(s => ({ providers: { ...s.providers, [deviceId]: providers } }));
  return providers;
}

async function listModels(deviceId: string, provider: string, force = false): Promise<ProviderModel[]> {
  const models = await call<ProviderModel[]>(deviceId, 'providers.models', { provider, force });
  patch(s => ({ models: { ...s.models, [`${deviceId}/${provider}`]: models } }));
  return models;
}

/** Starts a task on the host (repo OR projectId) and watches it. */
async function startTask(deviceId: string, params: StartParams): Promise<string> {
  if (!params.repo === !params.projectId) throw new Error('Choose a repository or a project.');
  const { sessionId } = await call<{ sessionId: string }>(deviceId, 'task.start', params, START_TIMEOUT_MS);
  watch(deviceId, [sessionId]);
  patch(s => ({ live: { ...s.live, [taskKey(deviceId, sessionId)]: { ...idleLive(), status: 'running', activity: 'Starting' } } }));
  return sessionId;
}

const sendMessage = (deviceId: string, sessionId: string, text: string) => call<{ queued: boolean }>(deviceId, 'task.send', { sessionId, text });
const stopTask = (deviceId: string, sessionId: string) => call<object>(deviceId, 'task.stop', { sessionId });
const steerTask = (deviceId: string, sessionId: string, text: string) => call<object>(deviceId, 'task.steer', { sessionId, text });
const configureTask = (deviceId: string, params: ConfigureParams) => call<object>(deviceId, 'task.configure', params);
/** An answered request leaves the live view at once, as on the desktop. */
async function answered(deviceId: string, sessionId: string, requestId: string, kind: 'approval_cancelled' | 'input_cancelled', reply: Promise<object>) {
  await reply;
  events(deviceId, [{ sessionId, kind, text: '', requestId }]);
  return {};
}
const respondApproval = (deviceId: string, sessionId: string, requestId: string, approve: boolean) =>
  answered(deviceId, sessionId, requestId, 'approval_cancelled', call(deviceId, 'approval.respond', { sessionId, requestId, approve }));
const respondInput = (deviceId: string, sessionId: string, requestId: string, response: unknown) =>
  answered(deviceId, sessionId, requestId, 'input_cancelled', call(deviceId, 'input.respond', { sessionId, requestId, response }));

export const connect = {
  init, signInStart, signInFinish, signInCancel, signOut,
  refreshDevices, setTrusted, revokeDevice,
  watch, unwatch, loadMessages,
  listTasks, listProviders, listModels, startTask,
  sendMessage, stopTask, steerTask, configureTask, respondApproval, respondInput,
};
