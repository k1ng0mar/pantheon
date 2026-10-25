// App state for the UI. Only src/connect/client.ts writes it; screens read it with the hooks below.
import { useMemo } from 'react';
import { create } from 'zustand';
import { useShallow } from 'zustand/react/shallow';
import { build } from '../connect/config.ts';
import { fingerprint } from '../connect/crypto.ts';
import { isTrusted, type Pins } from '../connect/trust.ts';
import type { Account, Device, Message, Provider, ProviderModel, TasksList } from '../connect/types.ts';
import { idleLive, type Live } from './reduce.ts';

export type { Live, LiveItem } from './reduce.ts';

/** `${deviceId}/${sessionId}` */
export const taskKey = (deviceId: string, sessionId: string) => `${deviceId}/${sessionId}`;

export interface ConnectState {
  configured: boolean;
  /** Secrets have been read; until then signedIn is unknown. */
  ready: boolean;
  signedIn: boolean;
  connected: boolean;
  account: Account | null;
  deviceId: string;
  /** This phone's public key fingerprint, to compare on the desktop. */
  fingerprint: string;
  devices: Device[];
  pins: Pins;
  /** tasks.list per host. */
  tasks: Record<string, TasksList & { loadedAt: number }>;
  /** Messages per taskKey, oldest first, incremental by afterId. */
  messages: Record<string, Message[]>;
  /** Live run per taskKey. */
  live: Record<string, Live>;
  providers: Record<string, Provider[]>;
  models: Record<string, ProviderModel[]>;
  /** The last user-facing connection error (sign-out, relay). */
  error: string | null;
}

export const initialState = (): ConnectState => ({
  configured: !!build, ready: false, signedIn: false, connected: false, account: null, deviceId: '', fingerprint: '',
  devices: [], pins: {}, tasks: {}, messages: {}, live: {}, providers: {}, models: {}, error: null,
});

export const useConnect = create<ConnectState>(() => initialState());

export const setPublicKey = (publicKey: string) => useConnect.setState({ fingerprint: fingerprint(publicKey) });

export interface DeviceRow extends Device { fingerprint: string; trusted: boolean; current: boolean }

function useRows(): DeviceRow[] {
  const { devices, pins, deviceId } = useConnect(useShallow(s => ({ devices: s.devices, pins: s.pins, deviceId: s.deviceId })));
  return useMemo(() => devices.map(d => ({ ...d, fingerprint: fingerprint(d.publicKey), trusted: isTrusted(pins, d), current: d.id === deviceId })), [devices, pins, deviceId]);
}

const EMPTY: never[] = [];

export const useStatus = () => useConnect(useShallow(s => ({ configured: s.configured, ready: s.ready, signedIn: s.signedIn, connected: s.connected, account: s.account, fingerprint: s.fingerprint, error: s.error })));
/** Every device on the account (for Settings → trust). */
export const useDevices = useRows;
/** Other trusted, hosting computers: the ones the phone can open. */
export function useHosts(): DeviceRow[] {
  const all = useRows();
  return useMemo(() => all.filter(d => !d.current && d.host && d.trusted), [all]);
}
export const useTasks = (deviceId: string) => useConnect(s => s.tasks[deviceId]);
export const useMessages = (deviceId: string, sessionId: string) => useConnect(s => s.messages[taskKey(deviceId, sessionId)] ?? EMPTY);
const IDLE = idleLive();
export const useLive = (deviceId: string, sessionId: string) => useConnect(s => s.live[taskKey(deviceId, sessionId)] ?? IDLE);
export const useProviders = (deviceId: string) => useConnect(s => s.providers[deviceId]);
export const useModels = (deviceId: string, provider: string) => useConnect(s => s.models[`${deviceId}/${provider}`]);
