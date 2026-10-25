// Pantheon neutral-surface tokens: the one-hue dial (plan doc §11)
// applied to the app's background/ink family. Accent-family variables are
// owned by palette.ts (accent swatches/wallpaper/VS Code themes), which
// consumes pantheonAccentSource() for the "Pantheon" accent choice; the two
// layers stay disjoint so neither can erase the other.
//
// Computed in TypeScript because the stylesheets' neutral values are plain
// hex and there is no CSS way to keep one dial driving eight variables plus
// the `rgb(var(--accent-rgb) / α)` triplets palette.ts already emits.

const STORAGE_KEY = 'pantheon.theme-tokens';

// Pantheon purple (#7223ff family): hue 262, barely-there tint on neutrals.
export const DEFAULT_TINT = { hue: 262, saturation: 10 };

const HUE_MIN = 0;
const HUE_MAX = 360;
const SAT_MIN = 0;
const SAT_MAX = 45;
/** Accent chroma the dial offers to palette.ts; stronger than surfaces by design. */
const ACCENT_SAT_DEFAULT = 84;

export interface PantheonTint {
  hue: number;
  saturation: number;
}

type Rgb = { r: number; g: number; b: number };

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function hslToRgb(h: number, s: number, l: number): Rgb {
  const hue = ((h % 360) + 360) % 360;
  const chroma = (1 - Math.abs(2 * l - 1)) * s;
  const scaled = hue / 60;
  const x = chroma * (1 - Math.abs((scaled % 2) - 1));
  let r = 0;
  let g = 0;
  let b = 0;
  if (scaled < 1) [r, g, b] = [chroma, x, 0];
  else if (scaled < 2) [r, g, b] = [x, chroma, 0];
  else if (scaled < 3) [r, g, b] = [0, chroma, x];
  else if (scaled < 4) [r, g, b] = [0, x, chroma];
  else if (scaled < 5) [r, g, b] = [x, 0, chroma];
  else [r, g, b] = [chroma, 0, x];
  const m = l - chroma / 2;
  return {
    r: Math.round((r + m) * 255),
    g: Math.round((g + m) * 255),
    b: Math.round((b + m) * 255),
  };
}

function hex({ r, g, b }: Rgb): string {
  const part = (v: number) => clamp(v, 0, 255).toString(16).padStart(2, '0');
  return `#${part(r)}${part(g)}${part(b)}`;
}

export function loadPantheonTint(): PantheonTint {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    const parsed = raw ? (JSON.parse(raw) as Partial<PantheonTint>) : null;
    return {
      hue: clamp(Math.round(parsed?.hue ?? DEFAULT_TINT.hue), HUE_MIN, HUE_MAX),
      saturation: clamp(Math.round(parsed?.saturation ?? DEFAULT_TINT.saturation), SAT_MIN, SAT_MAX),
    };
  } catch {
    return { ...DEFAULT_TINT };
  }
}

export function setPantheonTint(next: Partial<PantheonTint>): PantheonTint {
  const merged = { ...loadPantheonTint(), ...next };
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(merged));
  } catch { /* The dial still works when storage is unavailable. */ }
  applyPantheonTheme();
  window.dispatchEvent(new Event('pantheon-theme-change'));
  return merged;
}

/** The dial's accent color for palette.ts's "Pantheon" accent choice. */
export function pantheonAccentSource(): string {
  const { hue } = loadPantheonTint();
  return hex(hslToRgb(hue, ACCENT_SAT_DEFAULT / 100, 0.58));
}

/** Set the eight neutral variables the stylesheets already consume. */
export function applyPantheonTheme(): void {
  const { hue, saturation } = loadPantheonTint();
  const dark = document.documentElement.dataset.theme !== 'light';
  const n = (l: number) => hex(hslToRgb(hue, saturation / 100, l / 100));
  const vars: Record<string, string> = dark
    ? {
        '--bg': n(11), '--sidebar': n(13), '--panel': n(13), '--raised': n(19),
        '--border': n(22), '--text': n(93), '--muted': n(69), '--faint': n(61),
      }
    : {
        '--bg': n(100), '--sidebar': n(96), '--panel': n(98.5), '--raised': n(94),
        '--border': n(89), '--text': n(14), '--muted': n(39), '--faint': n(43),
      };
  const root = document.documentElement;
  for (const [name, value] of Object.entries(vars)) {
    root.style.setProperty(name, value);
  }
  const bg = root.style.getPropertyValue('--bg');
  if (bg) {
    document.querySelector('meta[name="theme-color"]')?.setAttribute('content', bg);
  }
}

// Neutrals carry the dial, so re-apply whenever light/dark flips.
window.addEventListener('pantheon-theme-change', () => applyPantheonTheme());
