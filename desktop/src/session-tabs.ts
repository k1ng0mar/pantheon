import './session-tabs.css';
import { escape, icon } from './dom';
import { providerLogo } from './provider-logos';
import type { Session } from './types';

/* Session tabs (plan doc §11): every open task is a tab over
 * the conversation column. Open state is UI-level — persisted, LRU-capped —
 * and closing a tab never touches the task itself; the sidebar stays the
 * full history. */

const STORE = 'pantheon.open-tabs';
const CAP = 12;

type Deps = {
  /** Activate the task (same path as selecting it in the sidebar). */
  select(id: string): void;
  /** A tab's close was requested; main removes it, falls back if active, and re-renders. */
  close(id: string, active: boolean): void;
};

let open: string[] = load();
let deps: Deps | null = null;
let strip: HTMLElement | null = null;

function load(): string[] {
  try {
    const raw = JSON.parse(localStorage.getItem(STORE) ?? '[]') as unknown;
    return Array.isArray(raw) ? raw.filter((id): id is string => typeof id === 'string').slice(0, CAP) : [];
  } catch { return []; }
}

function persist(): void {
  try { localStorage.setItem(STORE, JSON.stringify(open)); } catch { /* This launch keeps the tabs. */ }
}

/** Remember a task as open. Insertion order is stable; the oldest tab past the cap yields its seat. */
export function ensureTab(id: string): void {
  if (!id) return;
  if (open.includes(id)) return;
  open = [...open, id].slice(-CAP);
  persist();
}

export function closeTab(id: string): void {
  open = open.filter(tab => tab !== id);
  persist();
}

export function openTabs(): string[] {
  return [...open];
}

/** Tabs pointing at tasks that no longer exist (deleted, imported away) drop off. */
function prune(sessions: Session[]): void {
  const known = new Set(sessions.map(s => s.id));
  const next = open.filter(id => known.has(id));
  if (next.length !== open.length) {
    open = next;
    persist();
  }
}

export function createSessionTabs(deps_: Deps): void {
  deps = deps_;
  strip = document.querySelector<HTMLElement>('#session-tabs');
}

/** Redraw the strip. Cheap enough to call from the main render cycle. */
export function refreshSessionTabs(sessions: Session[], activeId: string, approvalsFor: (id: string) => number): void {
  if (!strip) return;
  prune(sessions);
  if (!open.length) {
    strip.hidden = true;
    strip.innerHTML = '';
    return;
  }
  const byId = new Map(sessions.map(s => [s.id, s]));
  const focused = document.activeElement;
  const refocus = focused instanceof HTMLElement && strip.contains(focused) ? focused.getAttribute('data-id') : null;
  strip.hidden = false;
  strip.innerHTML = open.map(id => {
    const s = byId.get(id);
    if (!s) return '';
    const running = s.status === 'running';
    const pending = approvalsFor(id);
    const tab = `<div class="session-tab${id === activeId ? ' active' : ''}" role="tab" tabindex="0" aria-selected="${id === activeId}" data-id="${escape(id)}" title="${escape(s.worktreeBranch ? `${s.title} — ${s.worktreeBranch}` : s.title)}">`
      + providerLogo(s.provider, 'session-tab-logo')
      + `<span class="session-tab-title">${escape(s.title || 'Task')}</span>`
      + (running ? '<span class="session-tab-run" role="img" aria-label="running"></span>' : '')
      + (pending > 0 ? `<span class="session-tab-badge" aria-label="${pending} pending">${pending > 9 ? '9+' : pending}</span>` : '')
      + `<button type="button" class="session-tab-close" data-close="${escape(id)}" aria-label="Close ${escape(s.title || 'tab')}">${icon('close')}</button>`
      + '</div>';
    return tab;
  }).join('');
  if (refocus) {
    const again = strip.querySelector<HTMLElement>(`.session-tab[data-id="${CSS.escape(refocus)}"]`);
    again?.focus();
  }
}

function handleClick(event: MouseEvent): void {
  const target = event.target as HTMLElement;
  const closeButton = target.closest<HTMLElement>('[data-close]');
  if (closeButton) {
    const id = closeButton.dataset.close!;
    const active = Boolean(strip?.querySelector(`.session-tab[data-id="${CSS.escape(id)}"]`)?.classList.contains('active'));
    deps?.close(id, active);
    return;
  }
  const tab = target.closest<HTMLElement>('.session-tab');
  if (tab?.dataset.id && deps) deps.select(tab.dataset.id);
}

// Mount once the shell exists; the pane is rendered by main's template.
const mount = () => {
  const pane = document.querySelector<HTMLElement>('.conversation-pane');
  const element = document.querySelector<HTMLElement>('#session-tabs');
  if (pane && element) {
    element.addEventListener('click', handleClick);
    element.addEventListener('auxclick', event => {
      if (event.button !== 1) return;
      const tab = (event.target as HTMLElement).closest<HTMLElement>('.session-tab');
      if (tab?.dataset.id) deps?.close(tab.dataset.id, tab.classList.contains('active'));
    });
    element.addEventListener('keydown', event => {
      const tab = (event.target as HTMLElement).closest<HTMLElement>('.session-tab');
      if (!tab?.dataset.id) return;
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        if (deps) deps.select(tab.dataset.id);
      } else if (event.key === 'Delete') {
        event.preventDefault();
        deps?.close(tab.dataset.id, tab.classList.contains('active'));
      }
    });
    return true;
  }
  return false;
};
if (!mount()) {
  document.addEventListener('DOMContentLoaded', mount, { once: true });
}
