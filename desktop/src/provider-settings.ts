import type { ProviderId } from './types';
import { providerLogo } from './provider-logos';
import { call } from './bridge';

export interface Configuration {
  secureStorageAvailable: boolean;
  pantheonKeySaved: boolean;
  pantheonKeyFromEnvironment: boolean;
}

/**
 * Pantheon keeps its own credentials; this desktop only needs to know which
 * `pantheon` binary to launch, and whether that binary answers over ACP.
 */
export function mountProviderSettings(host: HTMLElement, savedSettings: () => Promise<unknown>, _named?: { harnesses: { id: ProviderId; name: string }[]; saved: (snapshot: import('./connections').ConnectionSnapshot) => void }): () => void {
  host.innerHTML = `<section class="connection-card" data-provider="pantheon">
    <h3>${providerLogo('pantheon')}Pantheon</h3>
    <p>Pantheon reads its model and credentials from its own runtime, so there is nothing to paste here.</p>
    <div class="setting-row"><div><strong>Executable</strong><small>Leave it empty to use the installed <code>pantheon</code> on your PATH.</small></div></div>
    <form class="provider-executable-form" data-provider="pantheon">
      <label for="provider-executable">Pantheon executable</label>
      <input id="provider-executable" name="executable" autocomplete="off" spellcheck="false" maxlength="4096" placeholder="Find automatically">
      <div class="provider-executable-actions"><button type="submit" class="secondary-button">Apply</button><button type="button" class="secondary-button" data-executable-clear>Use automatic</button></div>
      <p class="provider-executable-status" role="status" aria-live="polite"></p>
    </form>
  </section>`;
  const reason = (error: unknown) => String(error).replace(/^Error:\s*/, '');
  const form = host.querySelector<HTMLFormElement>('.provider-executable-form')!;
  const input = form.querySelector<HTMLInputElement>('#provider-executable')!;
  const status = form.querySelector<HTMLElement>('.provider-executable-status')!;
  const clear = form.querySelector<HTMLButtonElement>('[data-executable-clear]')!;
  let disposed = false, pending = false;

  const show = (value: string | null, message: string) => {
    input.value = value ?? '';
    status.textContent = message;
    clear.disabled = !value;
  };
  const save = async (path: string | null) => {
    if (pending || disposed) return;
    pending = true;
    form.querySelectorAll<HTMLButtonElement>('button').forEach(button => { button.disabled = true; });
    status.textContent = path ? 'Saving…' : 'Clearing…';
    try {
      const saved = await call<string | null>('provider_save_executable', { provider: 'pantheon', path });
      if (disposed) return;
      show(saved, saved ? 'Saved. New tasks launch this executable.' : 'Cleared. Pantheon is found automatically again.');
      await savedSettings();
    } catch (error) {
      if (!disposed) status.textContent = reason(error);
    } finally {
      pending = false;
      if (!disposed) {
        form.querySelector<HTMLButtonElement>('button[type="submit"]')!.disabled = false;
        clear.disabled = !input.value;
      }
    }
  };
  form.addEventListener('submit', event => { event.preventDefault(); void save(input.value.trim() || null); });
  clear.addEventListener('click', () => { void save(null); });
  call<string | null>('provider_executable', { provider: 'pantheon' })
    .then(value => { if (!disposed) show(value, value ? 'Saved. New tasks launch this executable.' : ''); })
    .catch(error => { if (!disposed) status.textContent = reason(error); });
  return () => { disposed = true; };
}
