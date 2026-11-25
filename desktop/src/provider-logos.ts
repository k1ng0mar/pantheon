import type { ProviderId } from './types';
import { escape } from './dom';

// Pantheon is the only agent the app ships, so it is the only mark.
const logos: Partial<Record<string, string>> = {
  pantheon: new URL('./assets/providers/pantheon.svg', import.meta.url).href,
};

/** Decorative provider mark. The surrounding control supplies its accessible name. */
export function providerLogo(id: ProviderId, cls = ''): string {
  const className = `provider-logo${cls ? ` ${escape(cls)}` : ''}`;
  const src = logos[id as string];
  if (!src) return '';
  return `<img class="${className}" data-provider="${escape(id as string)}" src="${escape(src)}" width="20" height="20" alt="" aria-hidden="true" draggable="false">`;
}
export const providerLogoUrl = (id: ProviderId): string | null => logos[id as string] ?? null;
export const hasProviderLogo = (id: ProviderId): boolean => Boolean(logos[id as string]);
