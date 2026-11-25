import type { Message, ProviderId } from './types.ts';
/** The agent this build ships. A handoff only exists between the agents an app can run. */
const providers = new Set(['pantheon']);
export interface Handoff { fromProvider: ProviderId; toProvider: ProviderId; summary: string; }
export function decodeHandoff(message: Message): Handoff | null {
  if (message.role !== 'handoff') return null;
  try {
    const value = JSON.parse(message.content);
    return providers.has(value.fromProvider) && providers.has(value.toProvider) && typeof value.summary === 'string'
      ? { fromProvider: value.fromProvider, toProvider: value.toProvider, summary: value.summary.slice(0, 1000) } : null;
  } catch { return null; }
}
/** A saved response keeps the author that wrote it, whoever that was. */
export function historicalProvider(message: Message, messages: Message[], current: ProviderId): ProviderId {
  // Saved responses remember the author that wrote them. Only a plain identifier
  // can be an author, so a corrupted row cannot inject a label into the header.
  if (message.authorProvider) return /^[a-z0-9][a-z0-9-]{0,39}$/i.test(message.authorProvider) ? message.authorProvider as ProviderId : current;
  if (message.id < 0) return current;
  for (const next of messages) {
    if (next.sessionId !== message.sessionId || next.id <= message.id) continue;
    const handoff = decodeHandoff(next);
    if (handoff) return handoff.fromProvider;
  }
  return current;
}
