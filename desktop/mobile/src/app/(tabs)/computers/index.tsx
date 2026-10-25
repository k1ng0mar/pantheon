import * as Haptics from 'expo-haptics';
import { router } from 'expo-router';
import { memo, useCallback } from 'react';
import { Alert, FlatList, Pressable, StyleSheet, Switch, Text, View } from 'react-native';
import { connect } from '../../../connect/client.ts';
import { useDevices, useStatus, type DeviceRow } from '../../../store/index.ts';
import { mono, useColors } from '../../../ui/theme.ts';

const Row = memo(function Row({ device }: { device: DeviceRow }) {
  const colors = useColors();
  const toggle = async (trusted: boolean) => {
    void Haptics.selectionAsync();
    try { await connect.setTrusted(device.id, trusted); } catch (e) { Alert.alert('Could not change trust', (e as Error).message); }
  };
  const open = device.trusted && device.host;
  return (
    <Pressable disabled={!open} onPress={() => router.navigate({ pathname: '/tasks', params: { device: device.id } })}
      style={({ pressed }) => [styles.row, { backgroundColor: colors.card, opacity: pressed ? 0.7 : 1 }]} accessibilityRole={open ? 'button' : undefined}>
      <View style={[styles.dot, { backgroundColor: device.online ? colors.green : colors.tertiary }]} accessibilityLabel={device.online ? 'Online' : 'Offline'} />
      <View style={styles.main}>
        <Text style={[styles.name, { color: colors.text }]} numberOfLines={1}>{device.name}</Text>
        <Text style={[styles.detail, { color: colors.secondary }]}>{device.platform}{device.online ? ' · online' : ' · offline'}</Text>
        <Text style={[styles.fingerprint, { color: colors.tertiary }]} selectable>{device.fingerprint}</Text>
        {!device.host && <Text style={[styles.detail, { color: colors.orange }]}>Remote control is off on this computer. Turn on Allow remote control in Settings → Remote access.</Text>}
      </View>
      <Switch value={device.trusted} onValueChange={toggle} accessibilityLabel={`Trust ${device.name}`} />
    </Pressable>
  );
});

export default function Computers() {
  const colors = useColors();
  const all = useDevices();
  const { connected } = useStatus();
  const devices = all.filter(d => !d.current);
  const refresh = useCallback(() => { connect.refreshDevices().catch(() => undefined); }, []);
  return (
    <FlatList
      style={{ backgroundColor: colors.grouped }}
      contentInsetAdjustmentBehavior="automatic"
      contentContainerStyle={styles.list}
      data={devices}
      keyExtractor={d => d.id}
      renderItem={({ item }) => <Row device={item} />}
      onRefresh={refresh}
      refreshing={false}
      ItemSeparatorComponent={() => <View style={styles.gap} />}
      ListEmptyComponent={<Text style={[styles.empty, { color: colors.secondary }]}>{connected ? 'No other devices on this account yet. Sign in to PandaCode Connect on your computer.' : 'Connecting…'}</Text>}
      ListFooterComponent={
        <Text style={[styles.footer, { color: colors.secondary }]}>
          Trust a computer here and trust this phone on that computer too (Settings → Remote access): computers answer only devices they trust. Compare the fingerprints on both screens.
        </Text>
      }
    />
  );
}

const styles = StyleSheet.create({
  list: { padding: 16 },
  row: { flexDirection: 'row', alignItems: 'center', gap: 12, borderRadius: 16, padding: 14 },
  dot: { width: 10, height: 10, borderRadius: 5 },
  main: { flex: 1, gap: 2 },
  name: { fontSize: 17, fontWeight: '600' },
  detail: { fontSize: 13 },
  fingerprint: { fontSize: 11, fontFamily: mono },
  gap: { height: 10 },
  empty: { fontSize: 15, textAlign: 'center', marginTop: 40 },
  footer: { fontSize: 13, lineHeight: 18, marginTop: 16, paddingHorizontal: 4 },
});
