// One task: transcript (stored messages + live run), pending requests, composer.
import { Stack, useLocalSearchParams } from 'expo-router';
import { useEffect } from 'react';
import { KeyboardAvoidingView, Platform, ScrollView, StyleSheet, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { connect } from '../../../../connect/client.ts';
import type { PendingRequest } from '../../../../connect/types.ts';
import { taskKey, useConnect } from '../../../../store/index.ts';
import { Composer } from '../../../../ui/Composer.tsx';
import { ApprovalCard, InputCard } from '../../../../ui/requests.tsx';
import { isActive } from '../../../../ui/status.tsx';
import { useColors } from '../../../../ui/theme.ts';
import { Transcript } from '../../../../ui/transcript.tsx';

const NONE: PendingRequest[] = [];

export default function TaskScreen() {
  const { device = '', session = '' } = useLocalSearchParams<{ device: string; session: string }>();
  const colors = useColors();
  const insets = useSafeAreaInsets();
  const task = useConnect(s => s.tasks[device]?.tasks.find(t => t.sessionId === session));
  // Narrow selectors: a streamed text delta changes none of these, so this screen, the
  // composer and the list stay put while only the live row re-renders.
  const key = taskKey(device, session);
  const liveStatus = useConnect(s => s.live[key]?.status ?? 'idle');
  const approvals = useConnect(s => s.live[key]?.approvals ?? NONE);
  const inputs = useConnect(s => s.live[key]?.inputs ?? NONE);
  const status = liveStatus === 'idle' && task && isActive(task.status) ? task.status : liveStatus;

  useEffect(() => {
    connect.watch(device, [session]);
    connect.loadMessages(device, session).catch(() => undefined);
    return () => connect.unwatch(device, [session]);
  }, [device, session]);

  const requests = approvals.length + inputs.length > 0;
  return (
    <View style={[styles.screen, { backgroundColor: colors.background }]}>
      <Stack.Screen options={{ title: task?.title || 'Task' }} />
      <KeyboardAvoidingView style={styles.screen} behavior={Platform.OS === 'ios' ? 'padding' : undefined} keyboardVerticalOffset={Platform.OS === 'ios' ? 100 : 0}>
        <Transcript deviceId={device} sessionId={session} />
        <View style={{ paddingBottom: Math.max(insets.bottom, 8) }}>
          {requests && (
            <ScrollView style={styles.requests} keyboardShouldPersistTaps="handled">
              {approvals.map(r => <ApprovalCard key={r.requestId} deviceId={device} sessionId={session} request={r} />)}
              {inputs.map(r => <InputCard key={r.requestId} deviceId={device} sessionId={session} request={r} />)}
            </ScrollView>
          )}
          <Composer deviceId={device} sessionId={session} running={isActive(status)} task={task} />
        </View>
      </KeyboardAvoidingView>
    </View>
  );
}

const styles = StyleSheet.create({
  screen: { flex: 1 },
  requests: { maxHeight: 360, flexGrow: 0 },
});
