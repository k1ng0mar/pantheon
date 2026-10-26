import type { AgentEvent, AgentInputResponse, Message, Provider, ProviderId, ProviderModel, Session, SubagentActivity } from './types';
/** The launcher has its own tiny bridge and never imports the workspace preview. */
export interface LaunchContext { projectId: string | null; provider: string; model: string; mode: string; reasoningEffort?: string | null }
export interface OverlayPreferences { enabled: boolean; shortcut: string; context: LaunchContext; error: string | null; modelSelection?: {provider:string;model:string;reasoningEffort?:string|null} | null }
export interface OverlayTask { id: string; sessionId: string; kind: 'task' | 'worker' | 'subagent'; title: string; projectId: string; projectName: string; provider: string; status: string; updatedAt: number; parentTitle?: string | null; parentSessionId?: string | null; workerId?: string | null; subagentId?: string | null }
export interface OverlaySnapshot { tasks: OverlayTask[]; projects: {id: string; name: string; path: string}[]; context: LaunchContext; activeCount: number; teamWorkerCount?: number }
export interface OverlayThread { messages: Message[]; hasOlder: boolean; liveText: string; status: string; provider?: ProviderId; subagent?: SubagentActivity | null; historyVersion?: number; historyUnchanged?: boolean }
export const nativeOverlay = '__TAURI_INTERNALS__' in window;
const initial: OverlayPreferences = { enabled: true, shortcut: 'CommandOrControl+Shift+Space', context: {projectId: 'preview-project', provider: 'pantheon', model: 'default', mode: 'full'}, error: null };
let previewPreferences = initial;
try { previewPreferences = JSON.parse(localStorage.getItem('pantheon.overlay-preview') || 'null') || initial; } catch { /* Preview storage is optional. */ }
function effectiveContext(): LaunchContext { return {...previewPreferences.context,...previewPreferences.modelSelection,mode:previewPreferences.context.mode ?? 'full'}; }
// Preview fixtures only. Native catalogs come from the installed harnesses.
const previewCatalogs: Record<ProviderId, ProviderModel[]> = {
  pantheon:[{id:'default',name:'Pantheon runtime model',description:'The model is configured in the Pantheon runtime.'}],
};
const previewProviders: Provider[] = [{id:'pantheon',name:'Pantheon',installed:true,path:'/sample/bin/pantheon',version:'Sample provider',models:[]}];
const tasks: OverlayTask[] = [
  {id:'preview-active', sessionId:'preview-active', kind:'task', title:'Polish the new onboarding', projectId:'preview-project', projectName:'Pantheon', provider:'pantheon', status:'running', updatedAt:Date.now()},
  {id:'preview-approval', sessionId:'preview-approval', kind:'task', title:'Build a responsive gallery', projectId:'preview-project', projectName:'Pantheon', provider:'pantheon', status:'waiting', updatedAt:Date.now()},
];
const threads = new Map<string, OverlayThread>();
function previewThread(sessionId: string, subagentId?: string): OverlayThread {
  const key = `${sessionId}:${subagentId || ''}`;
  let thread = threads.get(key);
  if (!thread) {
    const task = tasks.find(task => task.sessionId === sessionId && (task.subagentId || '') === (subagentId || ''));
    if (!task) throw new Error('This task is no longer available.');
    const response = 'I’m refining the onboarding flow, starting with the welcome screen and keyboard navigation.';
    thread = {messages:[{id:1, sessionId, role:'user', content:task.title, createdAt:Date.now()-30_000}], hasOlder:false, liveText: response, status:task.status};
    threads.set(key, thread);
  }
  return thread;
}
/** Subscribe without pulling the full workspace or its preview into the launcher. */
export async function overlayEvents(callback: (event: AgentEvent) => void): Promise<() => void> {
  if (nativeOverlay) return (await import('@tauri-apps/api/event')).listen<AgentEvent>('agent-event', event => callback(event.payload));
  const receive = (event: Event) => callback((event as CustomEvent<AgentEvent>).detail);
  window.addEventListener('pantheon-overlay-agent-event', receive);
  return () => window.removeEventListener('pantheon-overlay-agent-event', receive);
}
export async function overlaySnapshot(): Promise<OverlaySnapshot> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<OverlaySnapshot>('overlay_snapshot');
  }
  return { tasks, projects: [{id:'preview-project', name:'Pantheon', path:'/Users/you/Projects/pantheon'}], context: effectiveContext(), activeCount: tasks.filter(task => task.status === 'running').length };
}
export async function overlayOpen(sessionId: string, subagentId?: string | null): Promise<OverlayThread> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<OverlayThread>('overlay_open', { sessionId, subagentId: subagentId || null });
  }
  return previewThread(sessionId, subagentId || undefined);
}
export async function overlayRespond(sessionId: string, response: AgentInputResponse): Promise<void> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('overlay_respond', { sessionId, response });
  }
  const thread = previewThread(sessionId);
  thread.messages.push({id:Date.now(), sessionId, role:'user', content: JSON.stringify(response), createdAt:Date.now()});
}
export async function overlaySteer(sessionId: string, text: string): Promise<void> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('overlay_steer', { sessionId, text });
  }
  const thread = previewThread(sessionId);
  thread.messages.push({id:Date.now(), sessionId, role:'user', content:text, createdAt:Date.now()});
  thread.liveText = 'Working on it…';
  window.setTimeout(() => { thread.liveText = 'Updated the onboarding flow based on your feedback.'; window.dispatchEvent(new CustomEvent('pantheon-overlay-agent-event', { detail: { sessionId, kind: 'text', text: thread.liveText } as AgentEvent })); }, 400);
}
export async function overlayStop(sessionId: string): Promise<void> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('overlay_stop', { sessionId });
  }
  const thread = previewThread(sessionId);
  thread.status = 'idle';
  thread.liveText = 'Stopped.';
}
export async function overlayNew(context: LaunchContext): Promise<string> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<string>('overlay_new', { context });
  }
  const id = 'preview-new-' + Date.now().toString(36);
  tasks.unshift({id, sessionId:id, kind:'task', title:'New task', projectId:context.projectId||'preview-project', projectName:'Pantheon', provider:'pantheon', status:'running', updatedAt:Date.now()});
  threads.set(id + ':', {messages:[{id:1,sessionId:id,role:'assistant',content:'Ready. What should I do?',createdAt:Date.now()}], hasOlder:false, liveText:'', status:'running'});
  return id;
}
export async function overlayPrefs(): Promise<OverlayPreferences> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<OverlayPreferences>('overlay_prefs');
  }
  return previewPreferences;
}
export async function overlaySetPrefs(prefs: OverlayPreferences): Promise<void> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('overlay_set_prefs', { prefs });
  }
  previewPreferences = prefs;
  try { localStorage.setItem('pantheon.overlay-preview', JSON.stringify(prefs)); } catch { /* ignore */ }
}
export async function overlayModels(provider: string): Promise<ProviderModel[]> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<ProviderModel[]>('overlay_models', { provider });
  }
  return (previewCatalogs[provider as ProviderId] || []).map(m => ({...m}));
}
export async function overlayProviders(): Promise<Provider[]> {
  if (nativeOverlay) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<Provider[]>('overlay_providers');
  }
  return previewProviders.map(p => ({...p}));
}
export type { AgentInputResponse } from './types';

export async function overlayCall<T = unknown>(command: string, args?: unknown): Promise<T> {
  if (nativeOverlay) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<T>(command, args as any);
  }
  return {} as T;
}

export function syncOverlayContext(_ctx: LaunchContext): void {}
