// New task: computer, project, harness + model, effort, mode, worktree, prompt → task.start.
import * as Haptics from 'expo-haptics';
import { router, useLocalSearchParams } from 'expo-router';
import { useEffect, useState, type ReactNode } from 'react';
import { ActivityIndicator, Alert, ScrollView, StyleSheet, Switch, Text, TextInput, View } from 'react-native';
import { connect } from '../connect/client.ts';
import { useHosts, useProviders, useTasks } from '../store/index.ts';
import { Choice } from '../ui/Choice.tsx';
import { useModelOptions } from '../ui/models.ts';
import { Button } from '../ui/requests.tsx';
import { MODES } from '../ui/status.tsx';
import { useColors } from '../ui/theme.ts';

export default function NewTask() {
  const colors = useColors();
  const params = useLocalSearchParams<{ device?: string }>();
  const hosts = useHosts().filter(h => h.online);
  const [devicePick, setDevicePick] = useState(params.device);
  const deviceId = hosts.find(h => h.id === devicePick)?.id ?? hosts[0]?.id ?? '';
  const list = useTasks(deviceId);
  const recent = list?.tasks.find(t => !t.chat);
  // The user's picks; anything unpicked follows the last task on that computer.
  const [chosen, setChosen] = useState<{ projectId?: string; provider?: string; model?: string; effort?: string | null; mode?: string }>({});
  const [worktree, setWorktree] = useState(false);
  const [prompt, setPrompt] = useState('');
  const [starting, setStarting] = useState(false);
  const projects = list?.projects ?? [];
  const projectId = projects.find(p => p.id === chosen.projectId)?.id ?? recent?.projectId ?? projects[0]?.id ?? '';
  const [providerPick, setProviderPick] = useState<string>();
  const firstInstalled = useProviders(deviceId)?.find(p => p.installed)?.id;
  const effectiveProvider = providerPick ?? recent?.provider ?? firstInstalled ?? '';
  const options = useModelOptions(deviceId, effectiveProvider);
  const model = chosen.model ?? (recent && effectiveProvider === recent.provider ? recent.model : 'default');
  const effort = chosen.effort !== undefined ? chosen.effort : recent && model === recent.model ? recent.effort : options.defaultEffort(model);
  const mode = chosen.mode ?? recent?.mode ?? 'ask';

  useEffect(() => { if (deviceId && !list) connect.listTasks(deviceId).catch(() => undefined); }, [deviceId, list]);
  const pick = (change: typeof chosen) => setChosen(c => ({ ...c, ...change }));

  const efforts = options.efforts(model);
  const ready = !!(deviceId && projectId && effectiveProvider && prompt.trim()) && !starting;
  const start = async () => {
    setStarting(true);
    void Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Medium);
    try {
      const session = await connect.startTask(deviceId, { projectId, worktree, prompt: prompt.trim(), provider: effectiveProvider, model, mode, effort: efforts.some(e => e.value === effort) ? effort! : undefined });
      void connect.listTasks(deviceId).catch(() => undefined);
      router.dismiss();
      router.push({ pathname: '/tasks/[device]/[session]', params: { device: deviceId, session } });
    } catch (e) {
      setStarting(false);
      Alert.alert('Could not start the task', (e as Error).message);
    }
  };

  if (!hosts.length) return <Text style={[styles.empty, { color: colors.secondary }]}>No trusted computer is online.</Text>;
  return (
    <ScrollView style={{ backgroundColor: colors.grouped }} contentContainerStyle={styles.form} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic">
      <View style={[styles.group, { backgroundColor: colors.card }]}>
        <Row label="Computer"><Choice value={deviceId} options={hosts.map(h => ({ value: h.id, label: h.name }))} onChange={id => { setDevicePick(id); setChosen({}); setProviderPick(undefined); }} /></Row>
        <Row label="Project">{list ? <Choice value={projectId} options={list.projects.map(p => ({ value: p.id, label: p.name }))} onChange={id => pick({ projectId: id })} /> : <ActivityIndicator />}</Row>
        <Row label="Harness">{options.loaded ? <Choice value={effectiveProvider} options={options.harnesses} onChange={p => { setProviderPick(p); pick({ model: 'default', effort: null }); }} /> : <ActivityIndicator />}</Row>
        <Row label="Model"><Choice value={model} options={[{ value: 'default', label: 'Default' }, ...options.models.filter(m => m.value !== 'default')]} onChange={m => pick({ model: m, effort: options.defaultEffort(m) })} /></Row>
        {efforts.length > 0 && <Row label="Effort"><Choice value={effort ?? ''} options={efforts} onChange={e => pick({ effort: e })} /></Row>}
        <Row label="Permissions"><Choice value={mode} options={MODES.map(m => ({ value: m.id, label: m.label }))} onChange={m => pick({ mode: m })} /></Row>
        <Row label="New worktree" last><Switch value={worktree} onValueChange={setWorktree} /></Row>
      </View>
      <TextInput value={prompt} onChangeText={setPrompt} placeholder="What should the agent do?" placeholderTextColor={colors.tertiary as string} multiline autoFocus
        style={[styles.prompt, { backgroundColor: colors.card, color: colors.text }]} accessibilityLabel="Prompt" />
      {starting
        ? <View style={styles.starting}><ActivityIndicator /><Text style={{ color: colors.secondary }}>Starting on the computer…</Text></View>
        : <Button label="Start task" kind="primary" onPress={start} disabled={!ready} />}
    </ScrollView>
  );
}

function Row({ label, children, last }: { label: string; children: ReactNode; last?: boolean }) {
  const colors = useColors();
  return (
    <View style={[styles.row, !last && { borderBottomWidth: StyleSheet.hairlineWidth, borderColor: colors.separator }]}>
      <Text style={[styles.label, { color: colors.text }]}>{label}</Text>
      {children}
    </View>
  );
}

const styles = StyleSheet.create({
  form: { padding: 16, gap: 16 },
  group: { borderRadius: 16, paddingHorizontal: 14 },
  row: { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', minHeight: 48, gap: 12 },
  label: { fontSize: 16 },
  prompt: { borderRadius: 16, padding: 14, fontSize: 16, minHeight: 120, textAlignVertical: 'top' },
  starting: { flexDirection: 'row', gap: 8, alignItems: 'center', justifyContent: 'center' },
  empty: { fontSize: 15, textAlign: 'center', marginTop: 40 },
});
