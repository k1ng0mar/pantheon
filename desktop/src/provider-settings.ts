import type { ProviderId } from './types';
import { escape } from './dom';
import { providerLogo } from './provider-logos';

export interface Configuration {
  secureStorageAvailable: boolean;
  pantheonKeySaved: boolean;
  pantheonKeyFromEnvironment: boolean;
}

export function mountProviderSettings(host: HTMLElement, _saved: () => Promise<unknown>, _named?: { harnesses: { id: ProviderId; name: string }[]; saved: (snapshot: import('./connections').ConnectionSnapshot) => void }): () => void {
  host.innerHTML = `<section class="connection-card"><h3>${providerLogo('pantheon')}Pantheon</h3><p>Pantheon is configured through its CLI/runtime.</p></section>`;
  return () => {};
}
