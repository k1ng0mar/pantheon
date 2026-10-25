// Tasks on one computer, pinned first, then grouped by project. Search filters titles and projects.
import { router, Stack, useFocusEffect, useLocalSearchParams } from 'expo-router';
import { memo, useCallback, useMemo, useState } from 'react';
import { Pressable, RefreshControl, SectionList, StyleSheet, Text, View } from 'react-native';
import { connect } from '../../../connect/client.ts';
import type { TaskSummary } from '../../../connect/types.ts';
import { useHosts, useTasks } from '../../../store/index.ts';
import { Choice } from '../../../ui/Choice.tsx';
import { Icon } from '../../../ui/Icon.tsx';
import { ago, isActive, StatusBadge } from '../../../ui/status.tsx';
import { useColors } from '../../../ui/theme.ts';

const POLL_MS = 15_000;

const TaskRow = memo(function TaskRow({ deviceId, task }: { deviceId: string; task: TaskSummary }) {
  const colors = useColors();
  return (
    <Pressable onPress={() => router.push({ pathname: '/tasks/[device]/[session]', params: { device: deviceId, session: task.sessionId } })}
      style={({ pressed }) => [styles.row, { backgroundColor: pressed ? colors.fill : colors.card }]} accessibilityRole="button">
      <View style={styles.main}>
        <Text style={[styles.title, { color: colors.text }]} numberOfLines={2}>{task.pinned ? '📌 ' : ''}{task.title || 'Untitled task'}</Text>
        <Text style={[styles.meta, { color: colors.secondary }]} numberOfLines={1}>
          {[task.provider, task.model !== 'default' && task.model, task.branch, ago(task.lastPromptAt ?? task.updatedAt)].filter(Boolean).join(' · ')}
        </Text>
      </View>
      <StatusBadge status={task.status} />
    </Pressable>
  );
});

export default function Tasks() {
  const colors = useColors();
  const hosts = useHosts();
  const params = useLocalSearchParams<{ device?: string }>();
  const host = hosts.find(h => h.id === params.device) ?? hosts.find(h => h.online) ?? hosts[0];
  const deviceId = host?.id ?? '';
  const list = useTasks(deviceId);
  const [query, setQuery] = useState('');
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState('');

  const load = useCallback(async () => {
    if (!deviceId || !host?.online) return;
    try { await connect.listTasks(deviceId); setError(''); } catch (e) { setError((e as Error).message); }
  }, [deviceId, host?.online]);
  const anyActive = !!list?.tasks.some(t => isActive(t.status));
  useFocusEffect(useCallback(() => {
    void load();
    if (!anyActive) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load, anyActive]));
  const refresh = async () => { setRefreshing(true); await load(); setRefreshing(false); };

  const sections = useMemo(() => {
    const q = query.trim().toLowerCase();
    const tasks = (list?.tasks ?? []).filter(t => !q || t.title.toLowerCase().includes(q) || t.projectName.toLowerCase().includes(q));
    const pinned = tasks.filter(t => t.pinned);
    const groups = new Map<string, TaskSummary[]>();
    for (const t of tasks) if (!t.pinned) groups.set(t.chat ? 'Chats' : t.projectName, [...(groups.get(t.chat ? 'Chats' : t.projectName) ?? []), t]);
    return [...(pinned.length ? [{ title: 'Pinned', data: pinned }] : []), ...[...groups].map(([title, data]) => ({ title, data }))];
  }, [list, query]);

  const empty = !hosts.length ? 'Trust a computer in Computers to see its tasks.'
    : !host?.online ? `${host?.name} is offline.`
    : error || (list ? (query ? 'No matching tasks.' : 'No tasks on this computer yet.') : 'Loading…');

  return (
    <>
      <Stack.Screen options={{
        headerSearchBarOptions: { placeholder: 'Search tasks', onChangeText: e => setQuery(e.nativeEvent.text), hideWhenScrolling: true },
        headerRight: () => host?.online ? (
          <Pressable onPress={() => router.push({ pathname: '/new-task', params: { device: deviceId } })} accessibilityRole="button" accessibilityLabel="New task" hitSlop={10} style={styles.headerButton}>
            <Icon ios="square.and.pencil" android="edit_square" size={20} color={colors.tint} />
          </Pressable>
        ) : null,
      }} />
      <SectionList
        style={{ backgroundColor: colors.grouped }}
        contentInsetAdjustmentBehavior="automatic"
        contentContainerStyle={styles.list}
        sections={sections}
        keyExtractor={t => t.sessionId}
        renderItem={({ item }) => <TaskRow deviceId={deviceId} task={item} />}
        renderSectionHeader={({ section }) => <Text style={[styles.section, { color: colors.secondary }]}>{section.title}</Text>}
        stickySectionHeadersEnabled={false}
        ItemSeparatorComponent={() => <View style={[styles.separator, { backgroundColor: colors.separator }]} />}
        refreshControl={<RefreshControl refreshing={refreshing} onRefresh={refresh} />}
        ListHeaderComponent={hosts.length > 1 ? (
          <View style={styles.picker}>
            <Text style={[styles.meta, { color: colors.secondary }]}>Computer</Text>
            <Choice value={deviceId} options={hosts.map(h => ({ value: h.id, label: `${h.name}${h.online ? '' : ' (offline)'}` }))} onChange={device => router.setParams({ device })} />
          </View>
        ) : host ? <Text style={[styles.meta, styles.picker, { color: colors.secondary }]}>{host.name}{host.online ? '' : ' · offline'}</Text> : undefined}
        ListEmptyComponent={<Text style={[styles.empty, { color: error ? colors.red : colors.secondary }]}>{empty}</Text>}
      />
    </>
  );
}

const styles = StyleSheet.create({
  list: { paddingHorizontal: 16, paddingBottom: 24 },
  row: { flexDirection: 'row', alignItems: 'center', gap: 12, paddingHorizontal: 14, paddingVertical: 11 },
  main: { flex: 1, gap: 3 },
  title: { fontSize: 16, fontWeight: '500' },
  meta: { fontSize: 13 },
  section: { fontSize: 13, fontWeight: '600', textTransform: 'uppercase', marginTop: 20, marginBottom: 6, marginLeft: 14 },
  separator: { height: StyleSheet.hairlineWidth, marginLeft: 14 },
  picker: { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 4, paddingTop: 8 },
  empty: { fontSize: 15, textAlign: 'center', marginTop: 40 },
  headerButton: { paddingHorizontal: 6 },
});
