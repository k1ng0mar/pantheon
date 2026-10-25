# PandaCode mobile

Expo app (iOS + Android) that views and controls tasks running on your computers through
PandaCode Connect ([docs/remote-access.md](../docs/remote-access.md)). The phone never runs tasks.

```bash
npm install
EXPO_PUBLIC_PANTHEON_RELAY_URL=https://… EXPO_PUBLIC_PANTHEON_GITHUB_CLIENT_ID=… npx expo start
npm test          # node:test (crypto, request framing, message merge, live events)
npx tsc --noEmit
npx expo lint
```

Put the two values in `mobile/.env` (`EXPO_PUBLIC_PANTHEON_RELAY_URL=https://your-relay`, `EXPO_PUBLIC_PANTHEON_GITHUB_CLIENT_ID=…`; the desktop names `PANTHEON_RELAY_URL`/`PANTHEON_GITHUB_CLIENT_ID` work too) and restart Metro with `npx expo start --clear`: the values are read when Metro starts, so an already-running Metro keeps the old ones. Without them the sign-in screen says which value is missing or invalid (the relay must be `https://host`, no path). Connect API for screens:
[src/connect/README.md](src/connect/README.md).
