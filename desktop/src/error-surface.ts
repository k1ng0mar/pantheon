/** Global safety net for errors no call site handled, plus an on-screen report.
 *
 * The main workspace routes its own failures through `report`/`notify`; this
 * module catches what those miss (unhandled rejections, uncaught errors, and
 * Rust panics emitted as `app-error`) so a failure is never silent in any view.
 */
import { escape, icon } from './dom';
import { subscribe } from './bridge';
import { hideToast, showToast } from './motion';
import type { AppError } from './types';

export type { AppError };

interface Normalized {
  kind: string;
  message: string;
  detail: string;
  guidance: string;
}

const GUIDANCE = 'Pantheon could not finish that operation. Your work is saved; try again or reload the window.';

let toast: HTMLElement | null = null;
let retry: (() => void) | null = null;
let hideTimer = 0;

function normalize(error: unknown): Normalized {
  if (error && typeof error === 'object') {
    const e = error as { kind?: unknown; message?: unknown; location?: unknown; guidance?: unknown };
    const message = typeof e.message === 'string' && e.message
      ? e.message
      : typeof e.kind === 'string' && e.kind ? e.kind : String(error);
    return {
      kind: typeof e.kind === 'string' ? e.kind : 'error',
      message,
      detail: typeof e.location === 'string' ? e.location : '',
      guidance: typeof e.guidance === 'string' ? e.guidance : GUIDANCE,
    };
  }
  return { kind: 'error', message: String(error), detail: '', guidance: GUIDANCE };
}

/** Report an error that was not handled locally. Safe to call from anywhere. */
export function reportError(error: unknown, options: { retry?: () => void } = {}): void {
  try {
    const normalized = normalize(error);
    console.error('[pantheon]', normalized.message, normalized.detail);
    if (options.retry) retry = options.retry;
    render(normalized);
  } catch (cause) {
    console.error('[pantheon] could not report an error', cause);
  }
}

function render(error: Normalized): void {
  if (!toast) return;
  clearTimeout(hideTimer);
  // message/detail are escaped; icon() is a static trusted SVG.
  toast.innerHTML = `<span class="toast-message">${escape(error.message)}</span>`
    + (error.detail ? `<span class="toast-detail">${escape(error.detail)}</span>` : '')
    + (retry ? `<button type="button" class="secondary-button toast-retry">Retry</button>` : '')
    + `<button type="button" class="icon-button toast-dismiss" aria-label="Dismiss error" title="Dismiss">${icon('close')}</button>`;
  toast.hidden = false;
  showToast(toast);
  toast.querySelector('.toast-retry')?.addEventListener('click', () => {
    const run = retry;
    retry = null;
    dismiss();
    run?.();
  });
  toast.querySelector('.toast-dismiss')?.addEventListener('click', dismiss);
  hideTimer = window.setTimeout(dismiss, 12_000);
}

function dismiss(): void {
  if (!toast) return;
  clearTimeout(hideTimer);
  retry = null;
  void hideToast(toast!);
}

/** Install the process-wide handlers. Call before the view mounts. */
export function initErrorSurface(): void {
  if (toast) return;
  toast = document.createElement('div');
  toast.className = 'toast error-toast';
  toast.setAttribute('role', 'alert');
  toast.hidden = true;
  document.body.append(toast);
  window.addEventListener('unhandledrejection', event => {
    event.preventDefault();
    reportError(event.reason);
  });
  window.addEventListener('error', event => {
    if (!event.error) return; // resource loads report no Error; skip them
    event.preventDefault();
    reportError(event.error);
  });
  void subscribe('app-error', (error: AppError) => reportError(error));
}
