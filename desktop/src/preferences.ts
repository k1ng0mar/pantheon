/**
 * Keep preferences from earlier releases without replacing newer choices.
 *
 * Scope-in-key rule (docs/desktop-app-plan.md §12): every persisted key says
 * where it applies. Prefixes: `pantheon.` global window state; keys that
 * belong to a narrower scope embed it (`pantheon.project.<id>.…`,
 * `pantheon.model.<session>.…`). Anything cross-surface (connections,
 * provider settings) lives in the backend, never here.
 */
export function migratePreferences(storage: Storage): void {
  const marker = 'pantheon.preferences-migrated';
  if (storage.getItem(marker) === '1') return;
  // Compatibility prefixes, never displayed as product branding.
  const legacy = ['kingcode.', 'pantheon.'];
  const keys = Array.from({ length: storage.length }, (_, index) => storage.key(index));
  for (const key of keys.filter((name): name is string => name !== null)) {
    const source = legacy.find(prefix => key.startsWith(prefix));
    if (!source) continue;
    const next = 'pantheon.' + key.slice(source.length);
    const value = storage.getItem(key);
    if (value !== null && storage.getItem(next) === null) storage.setItem(next, value);
  }
  storage.setItem(marker, '1');
}
