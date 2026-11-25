/** System webviews expose the host platform without another native round trip. */
export const isMac = /Mac|iPhone|iPad|iPod/i.test(navigator.platform);
export const isWindows = /Win/i.test(navigator.platform);
export const commandKey = isMac ? '⌘' : 'Ctrl';

/** A compact key chord for labels: ⌘Z on macOS, Ctrl+Z everywhere else. */
export function chord(key: string): string {
  return isMac ? `⌘${key}` : `Ctrl+${key}`;
}

export function shortcut(key: string, shift = false): string {
  return `${commandKey}${shift ? isMac ? ' ⇧' : ' Shift' : ''} ${key}`;
}
