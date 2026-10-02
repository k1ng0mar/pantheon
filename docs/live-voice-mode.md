# Live voice mode (mobile) - design

Opt-in live voice conversation for the mobile client (the PWA / AG-UI chat
page served at `/agui`). The user speaks live; the agent hears a transcript
and replies in voice. **This is a feature, never the default** - text chat
remains the default everywhere. Barge-in (interrupting the agent mid-reply)
is explicitly future work and out of scope for v1.

## Transport

`GET /agui/voice/live` - a WebSocket endpoint on the gateway's existing
hand-rolled HTTP serve layer (`pantheon-runtime/src/serve.rs`, dispatched
like the current `/agui/voice/*` POST edge).

- **Auth**: the existing bearer-token check applies. Browsers cannot set
  custom headers on a WS handshake, so the token travels as a `?token=`
  query parameter, validated against `cfg.auth_token` with the same origin
  checks as the other `/agui` routes. No token → the handshake is refused
  with 401 before upgrade.
- **Upgrade**: tungstenite server-side accept on the `TcpStream`
  (tungstenite is already a `pantheon-gateway` dependency). The handler
  itself lives in `pantheon-gateway` (new `live_voice.rs`); the runtime
  stays transport-blind.
- **Why WS and not the existing POST edge**: live mode needs a persistent
  bidirectional stream (audio chunks up, transcript + audio chunks down,
  control events both ways). The POST edge stays for one-shot
  transcribe/speak.

## Wire protocol

### Audio format: 16 kHz mono 16-bit PCM, binary WS frames

Chunks of ~100 ms (3200 bytes) client → server; TTS audio server →
client in the same format. **PCM, not Opus**: no decoder dependency,
trivial to slice and measure energy for VAD, and utterance-bounded so
chattiness is contained. Revisit Opus if bandwidth ever matters.

### Client → server

| Frame | Meaning |
|---|---|
| text `{"type":"start"}` | begin an utterance |
| binary | PCM audio chunk (part of the open utterance) |
| text `{"type":"end"}` | end utterance → transcribe now |
| text `{"type":"stop"}` | end the live session |

### Server → client

| Frame | Meaning |
|---|---|
| text `{"type":"ready"}` | session accepted, pipeline armed |
| text `{"type":"transcript","text":"...","final":true}` | STT result for the utterance |
| text `{"type":"reply_text","text":"..."}` | agent's reply as text (always sent, even in voice mode) |
| binary | PCM audio chunk of the spoken reply |
| text `{"type":"audio_end"}` | reply audio finished |
| text `{"type":"busy"}` | audio received while a turn is in flight - ignored, not queued |
| text `{"type":"approval_needed","text":"..."}` | a tool approval parked; the turn pauses until approved via the normal (text) approval path |
| text `{"type":"error","code":"..."}` | machine-readable error; never includes key material |
| text `{"type":"end"}` | session closed by server (limit reached, backend lost, etc.) |

## Turn-taking (v1)

**Client-driven.** The client sends `start`, streams chunks, sends `end`
(tap-to-talk, or auto-`end` on client-side pause detection). Server-side
safety net: a simple energy-based VAD - if RMS stays below threshold for
`silence_timeout_ms` after speech was detected, the utterance auto-closes;
`max_utterance_secs` force-closes runaway utterances. While a turn
(STT → agent → TTS) is in flight, incoming audio is dropped with a
`busy` event - **no barge-in in v1**.

## Pipeline (per utterance)

1. Buffer PCM chunks (capped at `max_utterance_secs` of audio).
2. On `end` / silence-timeout / force-close: run STT through the
   existing `VoicePipes` double-gate from
   `crates/pantheon-gateway/src/channel_voice.rs` - the `[tools] voice`
   toggle **and** a present `[stt]` section must both hold, else the slot
   is `Disabled`/`Unavailable` and the session is refused up front.
3. Transcript (marked as live-transcribed so the agent knows the source)
   → agent turn via the same dispatcher path as `/agui/rpc` chat, so
   behavior matches text chat exactly.
4. Reply text → TTS through the `[tts]` backend, honoring configured
   voice options → PCM binary frames streamed back, then `audio_end`.
5. Temp files (backends that need them) are cleaned on **every** path,
   including client disconnect and abort mid-turn.

## Config

```toml
[tools]
voice = true        # master switch, set by the setup Tools screen

[stt]               # existing section
backend = "groq"
# ...

[tts]               # existing section
backend = "piper"
# ...

[voice]
live_enabled = false        # default OFF - the opt-in
live_max_session_secs = 600 # hard session cap
live_max_utterance_secs = 30
live_silence_timeout_ms = 1200
```

The live session is refused unless `[tools] voice` is on, `[stt]` and
`[tts]` are both present and constructible, **and**
`[voice] live_enabled = true`.

## Client UI

The AG-UI page (`WEB_UI` in `pantheon-runtime/src/serve.rs`) is the
mobile chat surface. Add a "Live" button that opens a call-style overlay:

- big mic button (tap to start/stop an utterance),
- live level meter from an `AnalyserNode`,
- transcript area showing the live transcript and the reply text,
- End button closing the session.

Capture via `getUserMedia` + `AudioWorklet`, downsampling to 16 kHz mono
PCM. Keep styling minimal and consistent with the existing page.

## Safety and limits

- Session cap, utterance cap, audio-byte caps; disconnect cleans up.
- Approvals are never auto-approved by voice: they surface as
  `approval_needed` text events and the turn pauses.
- No key material in logs, events, or error codes.
- Text chat is untouched and remains the default.

## Explicitly out of scope (v1)

- Barge-in / interruption handling.
- Provider-native streaming STT (Deepgram/AssemblyAI WS) - v1 uses the
  existing chunked `transcribe()` trait; streaming providers are
  preferred only in that the utterance path stays identical.
- Opus or any compressed wire format.
- Agent-facing voice tools (voice stays channels + mobile, per Umar).

## Verification limits (honest)

Live provider streaming, real mobile mic capture, and actual end-to-end
latency are not verifiable in this environment. Tests use fake STT/TTS
backends and fixtures only.
