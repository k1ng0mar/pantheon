import type { ContextUsage, ProviderId, ProviderModel } from './types';

/** Unknown IDs stay literal; never infer a version from a moving alias. */
export function fallbackModel(_provider: ProviderId, id = 'default'): ProviderModel {
  id ||= 'default';
  if (id === 'default') return { id, name: 'Default', description: 'Uses the model selected by your harness.' };
  return { id, name: id, description: '' };
}

export function modelChoices(provider: ProviderId, catalog: ProviderModel[], current = ''): ProviderModel[] {
  const models = new Map<string, ProviderModel>([['default', fallbackModel(provider)]]);
  for (const model of catalog) if (model.id) models.set(model.id, model);
  if (current && !models.has(current)) models.set(current, fallbackModel(provider, current));
  return [...models.values()];
}

export function tokenCount(value: number): string {
  return new Intl.NumberFormat('en', { notation: 'compact', maximumFractionDigits: 1 }).format(value);
}

export function contextSummary(usage?: ContextUsage | null, capacity?: number | null): { label: string; detail: string } | null {
  const window = usage?.contextWindow ?? capacity;
  if (usage) {
    const used = tokenCount(usage.usedTokens);
    return window
      ? { label: `${used} / ${tokenCount(window)} context`, detail: `${usage.usedTokens.toLocaleString()} of ${window.toLocaleString()} context tokens (${Math.round(usage.usedTokens / window * 100)}%). Last reported by the harness.` }
      : { label: `${used} tokens in context`, detail: `${usage.usedTokens.toLocaleString()} context tokens. Last reported by the harness; total capacity is unavailable.` };
  }
  return window ? { label: `${tokenCount(window)} context`, detail: `${window.toLocaleString()} token context window reported by the harness.` } : null;
}

export function parseContextUsage(value: unknown): ContextUsage | null {
  if (!value || typeof value !== 'object') return null;
  const { usedTokens, contextWindow } = value as Record<string, unknown>;
  if (typeof usedTokens !== 'number' || !Number.isSafeInteger(usedTokens) || usedTokens < 0 || usedTokens > 1_000_000_000) return null;
  if (contextWindow != null && (typeof contextWindow !== 'number' || !Number.isSafeInteger(contextWindow) || contextWindow <= 0 || contextWindow > 1_000_000_000)) return null;
  return { usedTokens, contextWindow: (contextWindow as number | null) ?? null };
}
