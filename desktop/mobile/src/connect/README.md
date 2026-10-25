# Connect API for screens

The phone is a viewer: it signs in, lists the user's computers, and sees, prompts and
controls their tasks. Screens read state with hooks from `src/store` and act with
`connect` from `src/connect/client.ts`. Nothing else should touch the socket or secrets.

`connect.init()` runs once in `src/app/_layout.tsx`.

## Hooks (`src/store/index.ts`)

| Hook | Returns |
|---|---|
| `useStatus()` | `{ configured, ready, signedIn, connected, account, fingerprint, error }`. `fingerprint` is this phone's key, to compare on the desktop. `error` is e.g. "signed out" or a relay error. |
| `useDevices()` | every device on the account: `Device & { fingerprint, trusted, current }` (Settings → trust) |
| `useHosts()` | other trusted computers with hosting on: what the phone can open (`online` tells if it answers now) |
| `useTasks(deviceId)` | `{ tasks, projects, loadedAt }` from the last `listTasks`, statuses and titles kept live, or `undefined` |
| `useMessages(deviceId, sessionId)` | stored messages, oldest first |
| `useLive(deviceId, sessionId)` | the live run: `{ status, activity, items, approvals, inputs, usage?, limited? }`. `items` are `text` / `tool` (ToolActivity) / `note` / `error`, in order. `approvals` and `inputs` are `{ requestId, text, data }`; `data` of an input is the AgentInputRequest (questions/form/url). |
| `useProviders(deviceId)`, `useModels(deviceId, provider)` | last fetched lists |
| `useConnect(selector)` | the raw zustand store, for anything else |

## Actions (`connect.*`, all async, throw `Error` with a user-facing message)

- Sign-in: `signInStart()` → `{ userCode, verificationUri, expiresIn, … }` (show the code, open the URL), then `signInFinish(code)` waits for GitHub and registers the phone. `signInCancel()`, `signOut()`.
- Devices: `setTrusted(deviceId, trusted)` pins the device's current key (trust it on the desktop too: hosts answer only trusted devices). `revokeDevice(deviceId)`. `refreshDevices()` only matters before the socket's first presence frame.
- Tasks: `listTasks(deviceId, limit?)`, `startTask(deviceId, { repo | projectId, branch?, worktree, prompt, provider, model, mode?, effort? })` → sessionId (waits up to 12.5 min; the new task is watched).
- Open a task: `watch(deviceId, [sessionId])` subscribes (renewed every 9 min and on presence), copies missed messages and streams events into `useLive`; `unwatch` when the screen goes. `loadMessages(deviceId, sessionId)` copies new messages on demand (follows `hasMore`).
- In a task: `sendMessage(…, text)` → `{ queued }`, `steerTask(…, text)`, `stopTask(…)`, `configureTask(deviceId, { sessionId, provider?, model?, effort?, mode? })`, `respondApproval(…, requestId, approve)`, `respondInput(…, requestId, response)`.
- Model pickers: `listProviders(deviceId, force?)`, `listModels(deviceId, provider, force?)`.

At a run end the host's messages are copied, then the live view goes idle (`stopped`/`failed` when the run was).

## Rules kept here

- Crypto is byte-for-byte `src-tauri/src/remote/crypto.rs` (`tests/crypto.test.ts` checks a vector from the Rust crates).
- Calls and frames go only to devices trusted on this phone whose key matches presence; anything else is dropped.
- A 401 or close code 4001 signs out only if it refused the current token. Tokens with < 7 days left are refreshed at connect and every 12 h.
- Socket: backoff 1 s → 60 s (reset after 30 s up), 60 s after close 4000, 30 s pings, closed after 30 s in the background, reconnected on foreground.
- Secrets (token, X25519 key, device id, account, pins) live in `expo-secure-store`, this device only.
