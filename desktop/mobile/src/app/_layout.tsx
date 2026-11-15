import '../connect/polyfill.ts';
import { DarkTheme, DefaultTheme, Stack, ThemeProvider } from 'expo-router';
import { useFonts } from 'expo-font';
import { useEffect } from 'react';
import { useColorScheme } from 'react-native';
import { connect } from '../connect/client.ts';
import { useStatus } from '../store/index.ts';

export default function RootLayout() {
  const colorScheme = useColorScheme();
  const { ready, signedIn } = useStatus();
  // Bundled Cascadia Code. If it ever fails to load the app still runs on the
  // platform monospace instead of sitting on a blank screen.
  const [fontReady, fontError] = useFonts({ 'Cascadia Code': require('../../assets/fonts/cascadia-code.woff2') });
  useEffect(() => { void connect.init(); }, []);
  if (!ready || (!fontReady && !fontError)) return null;
  return (
    <ThemeProvider value={colorScheme === 'dark' ? DarkTheme : DefaultTheme}>
      <Stack screenOptions={{ headerShown: false }}>
        <Stack.Screen name="index" />
        <Stack.Protected guard={signedIn}>
          <Stack.Screen name="(tabs)" />
          <Stack.Screen name="new-task" options={{ presentation: 'formSheet', sheetAllowedDetents: [0.75, 1], sheetGrabberVisible: true, headerShown: true, title: 'New task' }} />
        </Stack.Protected>
        <Stack.Protected guard={!signedIn}>
          <Stack.Screen name="sign-in" />
        </Stack.Protected>
      </Stack>
    </ThemeProvider>
  );
}
