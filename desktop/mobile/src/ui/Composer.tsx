// The task composer: one glass bar with the prompt, Send (queues while running, like the
// desktop's Enter), Steer (while running) and Stop, plus model/effort/mode menus.
import * as Haptics from 'expo-haptics';
import { useState } from 'react';
import { Alert, Pressable, ScrollView, StyleSheet, Text, TextInput, View } from 'react-native';
import { connect } from '../connect/client.ts';
import type { TaskSummary } from '../connect/types.ts';
import { Choice } from './Choice.tsx';
import { Glass } from './Glass.tsx';
import { Icon } from './Icon.tsx';
import { useModelOptions } from './models.ts';
import { MODES } from './status.tsx';
import { useColors } from './theme.ts';

type Settings = Pick<TaskSummary, 'provider' | 'model' | 'effort' | 'mode'>;

export function Composer({ deviceId, sessionId, running, task }: { deviceId: string; sessionId: string; running: boolean; task?: TaskSummary }) {
  const colors = useColors();
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState('');
  const act = async (kind: 'send' | 'steer' | 'stop') => {
    const prompt = text.trim();
    if (kind !== 'stop' && !prompt) return;
    setBusy(true);
    try {
      if (kind === 'stop') {
        void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Warning);
        await connect.stopTask(deviceId, sessionId);
      } else {
        void Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Light);
        if (kind === 'steer') await connect.steerTask(deviceId, sessionId, prompt);
        else setNote((await connect.sendMessage(deviceId, sessionId, prompt)).queued ? 'Queued for after this run' : '');
        setText('');
        if (kind === 'steer') setNote('Steering…');
      }
    } catch (error) {
      Alert.alert(kind === 'stop' ? 'Could not stop' : 'Could not send', (error as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const hasText = !!text.trim();
  return (
    <Glass style={styles.bar}>
      {!!task && <TaskSettings deviceId={deviceId} sessionId={sessionId} task={task} />}
      <View style={styles.row}>
        <TextInput
          value={text}
          onChangeText={t => { setText(t); if (note) setNote(''); }}
          placeholder={note || (running ? 'Queue a message or steer…' : 'Message the agent…')}
          placeholderTextColor={colors.tertiary as string}
          multiline
          style={[styles.input, { color: colors.text }]}
          accessibilityLabel="Message"
        />
        {running && hasText && <Round label="Steer" ios="arrow.uturn.right" android="alt_route" onPress={() => act('steer')} disabled={busy} />}
        {running && <Round label="Stop" ios="stop.fill" android="stop" onPress={() => act('stop')} disabled={busy} danger />}
        {(!running || hasText) && <Round label={running ? 'Queue' : 'Send'} ios="arrow.up" android="arrow_upward" onPress={() => act('send')} disabled={busy || !hasText} primary />}
      </View>
    </Glass>
  );
}

function Round({ label, ios, android, onPress, disabled, primary, danger }: { label: string; ios: Parameters<typeof Icon>[0]['ios']; android: Parameters<typeof Icon>[0]['android']; onPress: () => void; disabled?: boolean; primary?: boolean; danger?: boolean }) {
  const colors = useColors();
  const bg = primary ? colors.tint : danger ? colors.red : colors.fill;
  const fg = primary || danger ? colors.onTint : colors.text;
  return (
    <Pressable onPress={onPress} disabled={disabled} accessibilityRole="button" accessibilityLabel={label} hitSlop={6}
      style={({ pressed }) => [styles.round, { backgroundColor: bg, opacity: disabled ? 0.35 : pressed ? 0.7 : 1 }]}>
      <Icon ios={ios} android={android} size={16} color={fg} />
    </Pressable>
  );
}

function TaskSettings({ deviceId, sessionId, task }: { deviceId: string; sessionId: string; task: TaskSummary }) {
  const colors = useColors();
  const [settings, setSettings] = useState<Settings>({ provider: task.provider, model: task.model, effort: task.effort, mode: task.mode });
  const options = useModelOptions(deviceId, settings.provider);
  const configure = async (change: Partial<Settings>) => {
    const before = settings;
    setSettings(s => ({ ...s, ...change }));
    void Haptics.selectionAsync();
    try {
      await connect.configureTask(deviceId, { sessionId, ...change });
    } catch (error) {
      setSettings(before);
      Alert.alert('Could not change settings', (error as Error).message);
    }
  };
  const efforts = options.efforts(settings.model);
  return (
    <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerStyle={styles.settings} keyboardShouldPersistTaps="handled">
      {options.harnesses.length > 0 && <Choice value={settings.provider} options={options.harnesses} onChange={provider => provider !== settings.provider && configure({ provider, model: 'default', effort: null })} />}
      {options.models.length > 0
        ? <Choice value={settings.model} options={options.models} onChange={model => configure({ provider: settings.provider, model, effort: options.defaultEffort(model) })} />
        : <Text style={{ color: colors.secondary }}>{settings.model}</Text>}
      {efforts.length > 0 && <Choice value={settings.effort ?? ''} options={efforts} onChange={effort => configure({ effort })} />}
      <Choice value={settings.mode} options={MODES.map(m => ({ value: m.id, label: m.label }))} onChange={mode => configure({ mode })} />
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  bar: { borderRadius: 26, marginHorizontal: 10, paddingHorizontal: 12, paddingVertical: 8, gap: 4 },
  row: { flexDirection: 'row', alignItems: 'flex-end', gap: 8 },
  input: { flex: 1, fontSize: 16, maxHeight: 140, paddingVertical: 8 },
  round: { width: 36, height: 36, borderRadius: 18, alignItems: 'center', justifyContent: 'center' },
  settings: { gap: 12, alignItems: 'center', paddingVertical: 2 },
});
