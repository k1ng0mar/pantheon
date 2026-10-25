/** Sign-in required: the composer banner for a task whose provider auth failed. */
import { escape, icon } from './dom';

export function authBanner(providerName: string): string {
  return `<section class="limit-banner auth-banner" aria-label="Sign-in required">${icon('alert')}<span class="limit-status" role="status">Sign-in required · ${escape(providerName)}</span>
    <button type="button" class="limit-continue" data-action="settings-providers">Open provider settings</button><button type="button" class="icon-button" data-action="auth-dismiss" aria-label="Dismiss sign-in notice" title="Dismiss">${icon('close')}</button></section>`;
}
