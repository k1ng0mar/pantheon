import type { CacheAlignment, CapabilityEntry, CapabilityHarness, CapabilityInventory } from './capabilities-panel';

const harnesses: CapabilityHarness[] = [
  { id: 'claude', name: 'Claude Code' },
  { id: 'codex', name: 'Codex' },
  { id: 'kimi', name: 'Kimi Code' },
  { id: 'opencode', name: 'OpenCode' },
];

const entries: CapabilityEntry[] = [
  { harness: 'claude', kind: 'slash command', name: '/review', state: 'native', source: 'Harness command catalog', command: null },
  { harness: 'claude', kind: 'mcp server', name: 'Preview documentation', state: 'discovered', source: 'User config', command: 'node' },
  { harness: 'claude', kind: 'hook', name: 'PreToolUse', state: 'discovered', source: 'Claude settings', command: null },
  { harness: 'claude', kind: 'plugin', name: 'sample-review@local', state: 'discovered', source: 'Claude settings', command: null },
  { harness: 'codex', kind: 'slash command', name: '/review', state: 'native', source: 'Harness command catalog', command: null },
  { harness: 'codex', kind: 'skill', name: 'Project guidelines', state: 'discovered', source: 'User skills directory', command: null },
];

const inventory: CapabilityInventory = {
  harnesses,
  entries,
  matrix: Array.from(new Map(entries.map(entry => [entry.kind + '\u0000' + entry.name, { kind: entry.kind, name: entry.name }])).values())
    .map(row => ({
      ...row,
      states: harnesses.map(harness => ({
        harness: harness.id,
        state: entries.find(entry => entry.harness === harness.id && entry.kind === row.kind && entry.name === row.name)?.state ?? 'missing',
      })),
    })),
  warnings: [],
};

const cache: CacheAlignment = {
  taskProvider: null,
  harnesses: [
    { id: 'claude', name: 'Claude Code', instructionDelivery: 'cachedSystemPrompt', state: 'supported', cacheTokens: 'observed', cacheReadTokens: 54000, cacheCreationTokens: 1800, cacheTokenSource: 'Sample observed record' },
    { id: 'codex', name: 'Codex', instructionDelivery: 'messageInjection', state: 'partial', cacheTokens: 'observed', cacheReadTokens: 12000, cacheCreationTokens: 0, cacheTokenSource: 'Sample observed record' },
    { id: 'kimi', name: 'Kimi Code', instructionDelivery: 'messageInjection', state: 'partial', cacheTokens: 'not exposed', cacheReadTokens: null, cacheCreationTokens: null, cacheTokenSource: 'Not exposed' },
    { id: 'opencode', name: 'OpenCode', instructionDelivery: 'messageInjection', state: 'partial', cacheTokens: 'not exposed', cacheReadTokens: null, cacheCreationTokens: null, cacheTokenSource: 'Not exposed' },
  ],
};

/** Sample-only preview response. No local files, harnesses, or cache data are read here. */
export function previewCapabilities(command: string): CapabilityInventory | CacheAlignment {
  if (command === 'capabilities_inventory') return structuredClone(inventory);
  if (command === 'cache_alignment') return structuredClone(cache);
  throw new Error('Unknown capabilities preview command.');
}

