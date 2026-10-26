import type {ProviderModel} from './types';
import {fallbackModel} from './model-catalog';

// Browser fixtures only
export const openCodePreviewModels: ProviderModel[] = [
  {id:'default',name:'Default',description:'default'},
];
export const openCodePreviewKey = 'pantheon.preview.opencode-models';
export function previewOpenCodeSelection(): string[] {
  try {
    const stored = JSON.parse(localStorage.getItem(openCodePreviewKey) || '[]');
    return Array.isArray(stored) ? stored.filter((id): id is string => typeof id === 'string') : [];
  } catch { return []; }
}
export type OpenCodeModels = {models: ProviderModel[]; selectedModelIds: string[]; error: string | null};
/** Picker choices */
export function openCodeChoices({models, selectedModelIds, error}: OpenCodeModels): ProviderModel[] {
  if (!selectedModelIds.length) {
    if (error) throw new Error(error);
    return [fallbackModel('pantheon'), ...models.filter(model => model.id !== 'default')];
  }
  return [fallbackModel('pantheon'), ...selectedModelIds.map(id => models.find(model => model.id === id) ?? fallbackModel('pantheon', id))];
}
export function previewOpenCodeChoices(): ProviderModel[] {
  return openCodeChoices({models: openCodePreviewModels, selectedModelIds: previewOpenCodeSelection(), error: null});
}
