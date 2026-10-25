import { memo } from 'react';
import { ActivityIndicator, StyleSheet, Text, View } from 'react-native';
import type { TaskStatus } from '../connect/types.ts';
import { Icon } from './Icon.tsx';
import { useColors } from './theme.ts';

export const isActive = (status: TaskStatus) => status === 'running' || status === 'waiting';

/** Running spinner, waiting-for-you, failed/stopped/interrupted marks; idle shows nothing. */
export const StatusBadge = memo(function StatusBadge({ status }: { status: TaskStatus }) {
  const colors = useColors();
  if (status === 'running') return <ActivityIndicator size="small" accessibilityLabel="Running" />;
  if (status === 'waiting') return <Pill label="Waiting for you" color={colors.orange} />;
  if (status === 'failed') return <Icon ios="exclamationmark.circle.fill" android="error" size={18} color={colors.red} />;
  if (status === 'stopped' || status === 'interrupted') return <Icon ios="stop.circle" android="stop_circle" size={18} color={colors.secondary} />;
  return null;
});

function Pill({ label, color }: { label: string; color: import('react-native').ColorValue }) {
  return (
    <View style={[styles.pill, { borderColor: color }]}>
      <Text style={[styles.pillText, { color }]}>{label}</Text>
    </View>
  );
}

export function ago(ms: number): string {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return 'now';
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

export const MODES = [
  { id: 'ask', label: 'Standard' },
  { id: 'auto', label: 'Auto-approve edits' },
  { id: 'plan', label: 'Plan only' },
  { id: 'full', label: 'Full access' },
];

const styles = StyleSheet.create({
  pill: { borderWidth: 1, borderRadius: 999, paddingHorizontal: 8, paddingVertical: 2 },
  pillText: { fontSize: 12, fontWeight: '600' },
});
