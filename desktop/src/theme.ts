import { migratePreferences } from './preferences';
import { applyPantheonTheme } from './pantheon-theme';

try { migratePreferences(localStorage); } catch { /* Retry next launch if storage is unavailable. */ }

export type Theme = 'light' | 'dark';

export function currentTheme(): Theme {
  return document.documentElement.dataset.theme === 'light' ? 'light' : 'dark';
}

function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
  applyPantheonTheme();
}

export function setTheme(theme: Theme): void {
  applyTheme(theme);
  try { localStorage.setItem('pantheon.theme', theme); } catch { /* The toggle still works when storage is unavailable. */ }
  window.dispatchEvent(new Event('pantheon-theme-change'));
}

// Apply the saved palette before constructing the workspace.
let saved: Theme = 'dark';
try { if (localStorage.getItem('pantheon.theme') === 'light') saved = 'light'; } catch { /* Use the default palette. */ }
applyTheme(saved);
