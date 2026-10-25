// System colors: iOS semantic colors (follow light/dark by themselves), Material 3 baseline on Android.
import { Platform, PlatformColor, useColorScheme, type ColorValue } from 'react-native';

export interface Colors {
  background: ColorValue; grouped: ColorValue; card: ColorValue; fill: ColorValue;
  text: ColorValue; secondary: ColorValue; tertiary: ColorValue; separator: ColorValue;
  tint: ColorValue; onTint: ColorValue; green: ColorValue; orange: ColorValue; red: ColorValue;
}

const ios: Colors = {
  background: PlatformColor('systemBackground'), grouped: PlatformColor('systemGroupedBackground'),
  card: PlatformColor('secondarySystemGroupedBackground'), fill: PlatformColor('tertiarySystemFill'),
  text: PlatformColor('label'), secondary: PlatformColor('secondaryLabel'), tertiary: PlatformColor('tertiaryLabel'),
  separator: PlatformColor('separator'), tint: PlatformColor('systemBlue'), onTint: '#ffffff',
  green: PlatformColor('systemGreen'), orange: PlatformColor('systemOrange'), red: PlatformColor('systemRed'),
};

const light: Colors = {
  background: '#fdf8fd', grouped: '#f3edf7', card: '#ffffff', fill: '#e7e0ec', text: '#1d1b20', secondary: '#49454f',
  tertiary: '#79747e', separator: '#cac4d0', tint: '#6750a4', onTint: '#ffffff', green: '#386a20', orange: '#8b5000', red: '#b3261e',
};
const dark: Colors = {
  background: '#141218', grouped: '#141218', card: '#211f26', fill: '#36343b', text: '#e6e0e9', secondary: '#cac4d0',
  tertiary: '#938f99', separator: '#49454f', tint: '#d0bcff', onTint: '#381e72', green: '#9ccc65', orange: '#ffb870', red: '#f2b8b5',
};

export function useColors(): Colors {
  const scheme = useColorScheme();
  return Platform.OS === 'ios' ? ios : scheme === 'dark' ? dark : light;
}

export const mono = Platform.select({ ios: 'Menlo', default: 'monospace' });
