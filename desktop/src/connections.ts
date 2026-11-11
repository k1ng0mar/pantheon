// Named provider connections: several setups of one harness, each with its own
// executable, arguments, environment and config folder. Secret values go to the
// native secure store and never come back; the list only carries their names.
//
// The editor UI is provider-settings.ts; this module owns the shapes the app and
// the native side agree on.
import type { ProviderId } from './types';

export interface Connection { id: string; provider: ProviderId; name: string; executable?: string | null; args: string[]; env: Record<string, string>; secretEnv: string[]; configDir?: string | null; revision: number; }
export interface ConnectionSnapshot { connections: Connection[]; tasks: Record<string, string>; }
