// Liquid Glass where iOS 26 has it; a solid system surface elsewhere (older iOS, Android).
import { GlassView, isLiquidGlassAvailable } from 'expo-glass-effect';
import { View, type ViewProps } from 'react-native';
import { useColors } from './theme.ts';

export const hasGlass = isLiquidGlassAvailable();

export function Glass({ style, interactive, ...rest }: ViewProps & { interactive?: boolean }) {
  const colors = useColors();
  if (hasGlass) return <GlassView glassEffectStyle="regular" isInteractive={interactive} style={style} {...rest} />;
  return <View style={[{ backgroundColor: colors.card, borderWidth: 0.5, borderColor: colors.separator }, style]} {...rest} />;
}
