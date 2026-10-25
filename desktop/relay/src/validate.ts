import { MAX_NAME_CHARS, MAX_PLATFORM_CHARS } from "./limits.ts";

export interface DeviceInput {
  id: string;
  name: string;
  platform: string;
  publicKey: string;
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const CONTROL = /[\u0000-\u001f\u007f]/;

export const isUuid = (v: unknown): v is string => typeof v === "string" && UUID.test(v);

/** Standard base64 that decodes to exactly 32 bytes (canonical form). */
export function isPublicKey(v: unknown): v is string {
  if (typeof v !== "string" || v.length !== 44) return false;
  try {
    return btoa(atob(v)) === v && atob(v).length === 32;
  } catch {
    return false;
  }
}

const text = (v: unknown, max: number): v is string =>
  typeof v === "string" && v.trim().length > 0 && [...v].length <= max && !CONTROL.test(v);

export function parseDevice(v: unknown): DeviceInput | string {
  if (typeof v !== "object" || v === null) return "device is required";
  const d = v as Record<string, unknown>;
  if (!isUuid(d.id)) return "device id must be a UUID";
  if (!text(d.name, MAX_NAME_CHARS)) return `device name must be 1-${MAX_NAME_CHARS} characters`;
  if (!text(d.platform, MAX_PLATFORM_CHARS)) return `platform must be 1-${MAX_PLATFORM_CHARS} characters`;
  if (!isPublicKey(d.publicKey)) return "publicKey must be a base64 32-byte key";
  return { id: d.id.toLowerCase(), name: d.name.trim(), platform: d.platform.trim(), publicKey: d.publicKey };
}
