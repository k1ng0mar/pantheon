import type { ProviderId } from './types';
import { escape } from './dom';

const logos: Partial<Record<ProviderId, string>> = {
  pantheon: new URL('./assets/providers/pantheon.svg', import.meta.url).href,
};

/** Decorative provider mark. The surrounding control supplies its accessible name. */
export function providerLogo(id: ProviderId, cls = ''): string {
  const className = `provider-logo${cls ? ` ${escape(cls)}` : ''}`;
  const src = logos[id];
  if (!src) return '';
  return `<img class="${className}" data-provider="${id}" src="${escape(src)}" width="20" height="20" alt="" aria-hidden="true" draggable="false">`;
}
export const providerLogoUrl = (id: ProviderId): string | null => logos[id] ?? null;
export const hasProviderLogo = (id: ProviderId): boolean => Boolean(logos[id]);
export const providerLogoInline = (_id: ProviderId): string | null => null;
