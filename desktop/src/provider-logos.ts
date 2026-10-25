import type { ProviderId } from './types';
import { escape } from './dom';
import githubCopilotLogo from './assets/providers/github-copilot.svg?raw';
import clineLogo from './assets/providers/cline.svg?raw';

// Original provider assets; see docs/brand-assets.md for provenance.
// Harnesses without an asset (omp) show no mark; their name stands alone.
const logos: Partial<Record<ProviderId, string>> = {
  pantheon: new URL('./assets/providers/pantheon.svg', import.meta.url).href,
  claude: new URL('./assets/providers/claude.svg', import.meta.url).href,
  codex: new URL('./assets/providers/openai.svg', import.meta.url).href,
  kimi: new URL('./assets/providers/kimi.png', import.meta.url).href,
  opencode: new URL('./assets/providers/opencode.png', import.meta.url).href,
  glm: new URL('./assets/providers/glm.svg', import.meta.url).href,
  antigravity: new URL('./assets/providers/antigravity.png', import.meta.url).href,
  grok: new URL('./assets/providers/grok.png', import.meta.url).href,
  mimo: new URL('./assets/providers/mimo.png', import.meta.url).href,
  pi: new URL('./assets/providers/pi.png', import.meta.url).href,
  deepseek: new URL('./assets/providers/deepseek.png', import.meta.url).href,
  commandcode: new URL('./assets/providers/commandcode.png', import.meta.url).href,
  muse: new URL('./assets/providers/muse.svg', import.meta.url).href,
};

/** Decorative provider mark. The surrounding control supplies its accessible name. */
export function providerLogo(id: ProviderId, cls = ''): string {
  const className = `provider-logo${cls ? ` ${escape(cls)}` : ''}`;
  // Monochrome marks are inlined and drawn in the theme's text color.
  const inline = id === 'github-copilot' ? githubCopilotLogo : id === 'cline' ? clineLogo.replaceAll('#1C1C24', 'currentColor') : '';
  if (inline) {
    return inline.replace('<svg ', `<svg class="${className}" data-provider="${id}" aria-hidden="true" focusable="false" `);
  }
  const src = logos[id];
  if (!src) return '';
  return `<img class="${className}" data-provider="${id}" src="${escape(src)}" width="20" height="20" alt="" aria-hidden="true" draggable="false">`;
}
