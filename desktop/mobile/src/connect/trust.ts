// Key pinning (docs/remote-access.md, "Trust"): the phone talks only to devices the user
// trusted on the phone, and only while the pinned key equals the key presence lists now.
import { get, set } from './secrets.ts';
import type { Device } from './types.ts';

export type Pins = Readonly<Record<string, string>>;
const KEY = 'connect.pins';

export const isTrusted = (pins: Pins, device: Pick<Device, 'id' | 'publicKey'> | undefined) =>
  !!device && !!device.publicKey && pins[device.id] === device.publicKey;

export async function loadPins(): Promise<Pins> {
  try {
    const value = JSON.parse((await get(KEY)) ?? '{}');
    return value && typeof value === 'object' ? value : {};
  } catch {
    return {};
  }
}

/** Pins the device's current key, or removes the pin. Returns the new pins. */
export async function setTrusted(pins: Pins, device: Device, trusted: boolean): Promise<Pins> {
  const { [device.id]: _, ...rest } = pins;
  const next = trusted ? { ...rest, [device.id]: device.publicKey } : rest;
  await set(KEY, JSON.stringify(next));
  return next;
}

/** Drops pins for devices that are no longer on the account. */
export async function prunePins(pins: Pins, devices: Device[]): Promise<Pins> {
  const next = Object.fromEntries(Object.entries(pins).filter(([id]) => devices.some(d => d.id === id)));
  if (Object.keys(next).length !== Object.keys(pins).length) await set(KEY, JSON.stringify(next));
  return next;
}
