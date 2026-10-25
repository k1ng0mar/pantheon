// Transcript rows. Stored messages render once each (memoized by id/content); the live run is
// its own component reading useLive, so streamed deltas re-render only the streaming rows.
import { memo, useMemo, useState } from 'react';
import { ActivityIndicator, FlatList, Pressable, ScrollView, StyleSheet, Text, View } from 'react-native';
import type { Message, ToolActivity } from '../connect/types.ts';
import { useLive, useMessages, type LiveItem } from '../store/index.ts';
import { Icon } from './Icon.tsx';
import { Markdown } from './Markdown.tsx';
import { mono, useColors } from './theme.ts';

// Same blocks the desktop folds behind a user message (CONTEXT_BOUNDARY in src/main.ts).
const CONTEXT_BOUNDARY = /\n\n(?=(?:File|Document|Folder|GitHub|Browser element) context[ :(]|Attached (?:browser screenshot|image|mockup)[ :]|Bot brief:\n|Response style \(PandaCode token saver)/;
const MAX_OUTPUT = 6000;

function parseTool(content: string): ToolActivity {
  try {
    const t = JSON.parse(content);
    return { id: '', name: String(t.name ?? t.title ?? 'Tool'), status: t.status === 'failed' ? 'failed' : t.status === 'running' ? 'running' : 'completed', input: String(t.input ?? ''), output: String(t.output ?? '') };
  } catch {
    return { id: '', name: 'Tool', status: 'completed', input: '', output: content };
  }
}

export const ToolRow = memo(function ToolRow({ tool }: { tool: ToolActivity }) {
  const colors = useColors();
  const [open, setOpen] = useState(false);
  const summary = tool.input.split('\n', 1)[0].slice(0, 120);
  const color = tool.status === 'failed' ? colors.red : colors.secondary;
  return (
    <Pressable onPress={() => setOpen(o => !o)} style={styles.tool} accessibilityRole="button" accessibilityLabel={`${tool.name} ${tool.status}`} accessibilityState={{ expanded: open }}>
      <View style={styles.toolHead}>
        {tool.status === 'running' ? <ActivityIndicator size="small" />
          : <Icon ios={tool.status === 'failed' ? 'xmark.circle' : 'wrench.and.screwdriver'} android={tool.status === 'failed' ? 'cancel' : 'build'} size={14} color={color} />}
        <Text style={[styles.toolName, { color }]} numberOfLines={1}>{tool.name}<Text style={{ color: colors.tertiary }}>{summary ? `  ${summary}` : ''}</Text></Text>
        <Icon ios={open ? 'chevron.up' : 'chevron.down'} android={open ? 'expand_less' : 'expand_more'} size={12} color={colors.tertiary} />
      </View>
      {open && (
        <View style={[styles.toolBody, { backgroundColor: colors.fill }]}>
          {!!tool.input && <Code text={tool.input} />}
          {!!tool.output && <Code text={tool.output} />}
        </View>
      )}
    </Pressable>
  );
});

function Code({ text }: { text: string }) {
  const colors = useColors();
  const shown = text.length > MAX_OUTPUT ? `${text.slice(0, MAX_OUTPUT)}\n…` : text;
  return (
    <ScrollView horizontal showsHorizontalScrollIndicator={false}>
      <Text selectable style={[styles.code, { color: colors.text }]}>{shown}</Text>
    </ScrollView>
  );
}

function UserBubble({ content }: { content: string }) {
  const colors = useColors();
  const [showContext, setShowContext] = useState(false);
  const cut = content.search(CONTEXT_BOUNDARY);
  const prompt = cut < 0 ? content : content.slice(0, cut);
  return (
    <View style={[styles.user, { backgroundColor: colors.fill }]}>
      <Text selectable style={[styles.userText, { color: colors.text }]}>{prompt}</Text>
      {cut >= 0 && (
        <Pressable onPress={() => setShowContext(s => !s)} accessibilityRole="button">
          <Text style={[styles.meta, { color: colors.secondary }]}>{showContext ? content.slice(cut + 2) : 'Attached context'}</Text>
        </Pressable>
      )}
    </View>
  );
}

const MessageRow = memo(function MessageRow({ message }: { message: Message }) {
  const colors = useColors();
  if (message.role === 'user') return <UserBubble content={message.content} />;
  if (message.role === 'assistant') return <View style={styles.row}><Markdown text={message.content} /></View>;
  if (message.role === 'tool') return <ToolRow tool={parseTool(message.content)} />;
  return <Text style={[styles.note, { color: message.role === 'error' ? colors.red : colors.secondary }]}>{message.content}</Text>;
});

const LiveItemRow = memo(function LiveItemRow({ item, streaming }: { item: LiveItem; streaming: boolean }) {
  const colors = useColors();
  if (item.kind === 'text') return item.text ? <View style={styles.row}><Markdown text={item.text} streaming={streaming} /></View> : null;
  if (item.kind === 'tool') return <ToolRow tool={item.tool} />;
  return <Text style={[styles.note, { color: item.kind === 'error' ? colors.red : colors.secondary }]}>{item.text}</Text>;
});

/** The in-flight run: streamed text, tool activity, and the status line. */
const LiveRun = memo(function LiveRun({ deviceId, sessionId }: { deviceId: string; sessionId: string }) {
  const colors = useColors();
  const live = useLive(deviceId, sessionId);
  if (live.status !== 'running' && live.status !== 'waiting' && !live.limited) return null;
  return (
    <View>
      {live.items.map((item, i) => <LiveItemRow key={item.kind === 'tool' ? item.tool.id : i} item={item} streaming={i === live.items.length - 1} />)}
      {!!live.limited && <Text style={[styles.note, { color: colors.orange }]}>{live.limited.text}</Text>}
      {live.status === 'running' && (
        <View style={styles.activity}>
          <ActivityIndicator size="small" />
          <Text style={[styles.meta, { color: colors.secondary }]}>{live.activity || 'Working…'}</Text>
        </View>
      )}
    </View>
  );
});

const key = (m: Message) => String(m.id);
const render = ({ item }: { item: Message }) => <MessageRow message={item} />;

/** Inverted list: newest at the bottom, the live run below the last stored message. */
export function Transcript({ deviceId, sessionId, footer }: { deviceId: string; sessionId: string; footer?: number }) {
  const messages = useMessages(deviceId, sessionId);
  const data = useMemo(() => [...messages].reverse(), [messages]);
  return (
    <FlatList
      inverted
      data={data}
      keyExtractor={key}
      renderItem={render}
      ListHeaderComponent={<LiveRun deviceId={deviceId} sessionId={sessionId} />}
      contentContainerStyle={[styles.list, { paddingTop: footer ?? 0 }]}
      keyboardDismissMode="interactive"
      contentInsetAdjustmentBehavior="automatic"
      initialNumToRender={12}
      windowSize={11}
    />
  );
}

const styles = StyleSheet.create({
  list: { paddingHorizontal: 16, paddingBottom: 12 },
  row: { marginVertical: 4 },
  user: { alignSelf: 'flex-end', maxWidth: '88%', borderRadius: 18, paddingHorizontal: 14, paddingVertical: 9, marginVertical: 8 },
  userText: { fontSize: 16, lineHeight: 22 },
  meta: { fontSize: 13, marginTop: 2 },
  note: { fontSize: 14, marginVertical: 4 },
  tool: { marginVertical: 2 },
  toolHead: { flexDirection: 'row', alignItems: 'center', gap: 8, paddingVertical: 5 },
  toolName: { flex: 1, fontSize: 13, fontFamily: mono },
  toolBody: { borderRadius: 10, padding: 10, gap: 8, marginBottom: 4 },
  code: { fontFamily: mono, fontSize: 12, lineHeight: 16 },
  activity: { flexDirection: 'row', alignItems: 'center', gap: 8, marginVertical: 8 },
});
