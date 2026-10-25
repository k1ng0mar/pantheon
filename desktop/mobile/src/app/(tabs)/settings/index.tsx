import Constants from 'expo-constants';
import { Image } from 'expo-image';
import * as WebBrowser from 'expo-web-browser';
import type { ReactNode } from 'react';
import { Alert, Pressable, ScrollView, StyleSheet, Text, View } from 'react-native';
import { connect } from '../../../connect/client.ts';
import { useStatus } from '../../../store/index.ts';
import { mono, useColors } from '../../../ui/theme.ts';

const SOURCE = 'https://github.com/aicodeking/pandacode';

export default function Settings() {
  const colors = useColors();
  const status = useStatus();
  const signOut = () => Alert.alert('Sign out?', 'This phone stops seeing your computers until you sign in again.', [
    { text: 'Cancel', style: 'cancel' },
    { text: 'Sign out', style: 'destructive', onPress: () => void connect.signOut() },
  ]);
  return (
    <ScrollView style={{ backgroundColor: colors.grouped }} contentInsetAdjustmentBehavior="automatic" contentContainerStyle={styles.page}>
      <Section title="Account">
        <View style={styles.row}>
          {!!status.account?.avatarUrl && <Image source={status.account.avatarUrl} style={styles.avatar} />}
          <View style={{ flex: 1 }}>
            <Text style={[styles.value, { color: colors.text }]}>{status.account?.login ?? '—'}</Text>
            <Text style={[styles.small, { color: status.error ? colors.red : colors.secondary }]}>{status.error ?? (status.connected ? 'Connected' : 'Connecting…')}</Text>
          </View>
        </View>
      </Section>
      <Section title="This phone" footer="Compare this fingerprint with the one your computer shows in Settings → Remote access before trusting this phone there.">
        <Text selectable style={[styles.fingerprint, { color: colors.text }]}>{status.fingerprint}</Text>
      </Section>
      <Section>
        <Pressable onPress={signOut} accessibilityRole="button"><Text style={[styles.value, { color: colors.red }]}>Sign out</Text></Pressable>
      </Section>
      <Section title="About" footer="PandaCode is open source. This app has no telemetry; it talks only to the PandaCode Connect relay and GitHub sign-in.">
        <Text style={[styles.value, { color: colors.text }]}>PandaCode {Constants.expoConfig?.version ?? ''}</Text>
        <Pressable onPress={() => void WebBrowser.openBrowserAsync(SOURCE)} accessibilityRole="link"><Text style={[styles.value, { color: colors.tint }]}>Source code</Text></Pressable>
      </Section>
    </ScrollView>
  );
}

function Section({ title, footer, children }: { title?: string; footer?: string; children: ReactNode }) {
  const colors = useColors();
  return (
    <View style={styles.section}>
      {!!title && <Text style={[styles.header, { color: colors.secondary }]}>{title}</Text>}
      <View style={[styles.card, { backgroundColor: colors.card }]}>{children}</View>
      {!!footer && <Text style={[styles.small, styles.footer, { color: colors.secondary }]}>{footer}</Text>}
    </View>
  );
}

const styles = StyleSheet.create({
  page: { padding: 16, gap: 24 },
  section: { gap: 6 },
  header: { fontSize: 13, fontWeight: '600', textTransform: 'uppercase', marginLeft: 14 },
  card: { borderRadius: 16, padding: 14, gap: 12 },
  row: { flexDirection: 'row', alignItems: 'center', gap: 12 },
  avatar: { width: 40, height: 40, borderRadius: 20 },
  value: { fontSize: 17 },
  small: { fontSize: 13, lineHeight: 18 },
  footer: { marginHorizontal: 14 },
  fingerprint: { fontFamily: mono, fontSize: 14 },
});
