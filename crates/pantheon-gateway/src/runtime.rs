//! Gateway runtime merged group.

pub mod daemon {
    //! Channel daemon: standalone polling loop for surfaces.
    //!
    //! Telegram is long-polled (`getUpdates`) with the offset cursor kept in
    //! memory and persisted to disk so a restart does not replay the window.
    //! Discord's gateway is a websocket protocol; running one without an async
    //! runtime is out of scope here, so Discord inbound remains webhook-fed
    //! (`push_inbound`), which the same daemon loop drains.
    //!
    //! The daemon owns exactly one responsibility: move events from surfaces
    //! into the runtime (chat / grant / deny / cancel) and move frames back out.
    //! It contains no business logic.

    use crate::channels::channel::{Channel, ChannelEnvelope, ChannelError, ChannelEvent};
    use crate::channels::telegram::{TelegramChannel, TelegramRestTransport, TelegramTransport};
    use crate::voice_group::channel_voice::VoiceOutcome;
    use crate::OutboundMessage;
    use pantheon_api::logging::now_ms;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Send attempts per message before it is dead-lettered. A reply that can
    /// never be delivered (wrong chat id, revoked token, persistent 429) must
    /// not wedge the queue behind it forever: three strikes, then a log line.
    pub const MAX_SEND_ATTEMPTS: u32 = 3;

    /// Consecutive poll/send failures before a channel is marked dead.
    const HEALTH_DEAD_AFTER: u32 = 5;
    /// Health files are rewritten on state transition and at most this often.
    const HEALTH_WRITE_INTERVAL_MS: u64 = 30_000;
    /// A health file older than this means the daemon stopped reporting.
    const HEALTH_STALE_AFTER_MS: u64 = 120_000;

    /// Channel health state for `GET /api/health/channels` (F-7). The daemon
    /// reports; the dashboard reads via [`channel_health_json`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ChannelState {
        /// No poll/send outcome observed yet.
        Unknown,
        /// Last interaction succeeded.
        Connected,
        /// Recent failures, still retrying.
        Degraded,
        /// Auth failure, failure budget exhausted, or dead-lettered: needs an
        /// operator (fix the token / config, then the daemon recovers on the
        /// next success).
        Dead,
    }

    impl ChannelState {
        fn as_str(self) -> &'static str {
            match self {
                ChannelState::Unknown => "unknown",
                ChannelState::Connected => "connected",
                ChannelState::Degraded => "degraded",
                ChannelState::Dead => "dead",
            }
        }
    }

    /// One channel's health snapshot. Serialized to
    /// `channel-health-<name>.json` next to the cursor file.
    #[derive(Debug, Clone)]
    pub struct ChannelHealth {
        pub name: String,
        pub state: ChannelState,
        pub last_error: Option<String>,
        pub last_success_ms: Option<u64>,
        pub consecutive_failures: u32,
        pub updated_ms: u64,
    }

    /// True for errors that mean the credential is gone (401 from the
    /// platform), not that the network hiccuped: those go straight to dead,
    /// no failure-budget grace.
    fn is_auth_failure(detail: &str) -> bool {
        detail.contains("401") || detail.to_lowercase().contains("unauthorized")
    }

    /// Per-daemon health tracker (F-7). Call the `record_*` methods from the
    /// daemon loop; snapshots are written on state transition and at most
    /// every 30s so a crash mid-degradation still leaves evidence.
    pub struct ChannelHealthTracker {
        dir: PathBuf,
        states: HashMap<String, ChannelHealth>,
        last_write_ms: u64,
    }

    impl ChannelHealthTracker {
        pub fn new(dir: PathBuf) -> Self {
            let _ = std::fs::create_dir_all(&dir);
            Self {
                dir,
                states: HashMap::new(),
                last_write_ms: 0,
            }
        }

        fn entry(&mut self, name: &str) -> &mut ChannelHealth {
            let now = now_ms() as u64;
            self.states
                .entry(name.to_string())
                .or_insert(ChannelHealth {
                    name: name.to_string(),
                    state: ChannelState::Unknown,
                    last_error: None,
                    last_success_ms: None,
                    consecutive_failures: 0,
                    updated_ms: now,
                })
        }

        fn record_ok(&mut self, name: &str) {
            let now = now_ms() as u64;
            let h = self.entry(name);
            let transitioned = h.state != ChannelState::Connected;
            h.state = ChannelState::Connected;
            h.consecutive_failures = 0;
            h.last_error = None;
            h.last_success_ms = Some(now);
            h.updated_ms = now;
            self.maybe_write(transitioned);
        }

        fn record_err(&mut self, name: &str, detail: String) {
            let now = now_ms() as u64;
            let h = self.entry(name);
            let prev = h.state;
            if is_auth_failure(&detail) {
                h.state = ChannelState::Dead;
                h.consecutive_failures = HEALTH_DEAD_AFTER;
            } else {
                h.consecutive_failures += 1;
                h.state = if h.consecutive_failures >= HEALTH_DEAD_AFTER {
                    ChannelState::Dead
                } else {
                    ChannelState::Degraded
                };
            }
            let transitioned = prev != h.state;
            h.last_error = Some(detail);
            h.updated_ms = now;
            // The 30s throttle in maybe_write keeps the failure budget
            // visible even between transitions.
            self.maybe_write(transitioned);
        }

        /// Telegram long-poll batch succeeded (even an empty batch proves the
        /// connection works).
        pub fn record_poll_ok(&mut self, name: &str) {
            self.record_ok(name);
        }

        /// Telegram long-poll batch failed.
        pub fn record_poll_err(&mut self, name: &str, err: &str) {
            self.record_err(name, err.to_string());
        }

        /// A channel send succeeded.
        pub fn record_send_ok(&mut self, name: &str) {
            self.record_ok(name);
        }

        /// A channel send failed (non-rate-limit).
        pub fn record_send_err(&mut self, name: &str, err: &ChannelError) {
            if is_auth_failure(&err.to_string()) {
                self.record_err(name, format!("{} (credential rejected)", err));
            } else {
                self.record_err(name, err.to_string());
            }
        }

        /// A 429 is the platform asking us to slow down, not a channel fault:
        /// mark degraded without spending the failure budget.
        pub fn record_send_rate_limited(&mut self, name: &str, err: &ChannelError) {
            let now = now_ms() as u64;
            let h = self.entry(name);
            let transitioned = h.state != ChannelState::Degraded && h.state != ChannelState::Dead;
            if h.state != ChannelState::Dead {
                h.state = ChannelState::Degraded;
            }
            h.last_error = Some(err.to_string());
            h.updated_ms = now;
            self.maybe_write(transitioned);
        }

        /// A message exhausted [`MAX_SEND_ATTEMPTS`] and was dead-lettered.
        pub fn record_dead_letter(&mut self, name: &str, detail: &str) {
            let now = now_ms() as u64;
            let h = self.entry(name);
            let transitioned = h.state != ChannelState::Dead;
            h.state = ChannelState::Dead;
            h.consecutive_failures = HEALTH_DEAD_AFTER;
            h.last_error = Some(format!(
                "dead-lettered after {MAX_SEND_ATTEMPTS} attempts: {detail}"
            ));
            h.updated_ms = now;
            self.maybe_write(transitioned);
        }

        fn maybe_write(&mut self, transitioned: bool) {
            let now = now_ms() as u64;
            if transitioned || now.saturating_sub(self.last_write_ms) >= HEALTH_WRITE_INTERVAL_MS {
                self.write();
            }
        }

        fn write(&mut self) {
            for h in self.states.values() {
                let safe: String = h
                    .name
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let path = self.dir.join(format!("channel-health-{safe}.json"));
                let v = serde_json::json!({
                    "name": h.name,
                    "state": h.state.as_str(),
                    "last_error": h.last_error,
                    "last_success_ms": h.last_success_ms,
                    "consecutive_failures": h.consecutive_failures,
                    "updated_ms": h.updated_ms,
                });
                let _ = std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            self.last_write_ms = now_ms() as u64;
        }
    }

    /// Merge the daemon's per-channel health files for
    /// `GET /api/health/channels`. Files older than ~120s are marked stale
    /// (the daemon stopped reporting); a missing file means the daemon never
    /// reported that channel.
    pub fn channel_health_json(data_dir: &Path) -> serde_json::Value {
        let dir = data_dir.join("gateway");
        let mut channels = Vec::new();
        let now = now_ms() as u64;
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .map(|r| r.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        for entry in entries {
            let fname = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = fname.strip_prefix("channel-health-") else {
                continue;
            };
            if !rest.ends_with(".json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let updated = v.get("updated_ms").and_then(|u| u.as_u64()).unwrap_or(0);
            if now.saturating_sub(updated) > HEALTH_STALE_AFTER_MS {
                v["stale"] = serde_json::Value::Bool(true);
            }
            channels.push(v);
        }
        channels.sort_by(|a, b| {
            a.get("name")
                .and_then(|n| n.as_str())
                .cmp(&b.get("name").and_then(|n| n.as_str()))
        });
        serde_json::json!({ "channels": channels })
    }

    /// Where polled events go: implemented by the runtime bridge. `sender`
    /// is the platform sender identity when the surface exposes one; the
    /// gateway allowlist keys on it.
    pub trait EventSink: Send + Sync {
        /// A user message arrived.
        fn on_message(&self, thread_id: &str, sender: Option<&str>, text: &str);
        /// An approval button was clicked. `run_id` is `Some` when the
        /// callback carries one (`grant:{run_id}:{scope}`); `None` for the
        /// plain `grant:{scope}` format, where the sink falls back to its
        /// thread→run map.
        fn on_approval(
            &self,
            thread_id: &str,
            sender: Option<&str>,
            run_id: Option<&str>,
            scope: &str,
            grant: bool,
        );
    }

    /// Persisted `update_id` cursor so a daemon restart skips already-seen
    /// updates. Telegram offsets: pass `offset = last_id + 1`.
    #[derive(Debug, Default)]
    pub struct UpdateCursor {
        path: PathBuf,
        offset: Mutex<i64>,
    }

    impl UpdateCursor {
        pub fn new(path: PathBuf) -> Self {
            let offset = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(0);
            Self {
                path,
                offset: Mutex::new(offset),
            }
        }
        pub fn get(&self) -> i64 {
            *self.offset.lock().unwrap_or_else(|e| e.into_inner())
        }
        pub fn advance(&self, to: i64) {
            let mut o = self.offset.lock().unwrap_or_else(|e| e.into_inner());
            if to > *o {
                *o = to;
                // Best-effort persist: a lost cursor only replays the poll window.
                let _ = std::fs::write(&self.path, to.to_string());
            }
        }
    }

    /// Long-poll Telegram once through the transport trait. Returns normalized
    /// events and the next offset (highest update_id + 1). A transport error is
    /// returned as Err so the caller decides the backoff; it never panics the
    /// daemon.
    pub fn poll_telegram_once(
        transport: &dyn TelegramTransport,
        offset: i64,
        timeout_secs: u64,
    ) -> Result<(Vec<ChannelEvent>, i64), String> {
        let updates = transport
            .get_updates(offset, timeout_secs)
            .map_err(|e| e.to_string())?;
        Ok(collect_telegram_events(&updates, offset))
    }

    /// Fold polled updates into channel events + the next offset. Pure:
    /// unit-tested directly with inline update JSON, no transport involved.
    pub fn collect_telegram_events(updates: &[Value], offset: i64) -> (Vec<ChannelEvent>, i64) {
        let mut events = Vec::new();
        let mut highest = offset;
        for update in updates {
            let id = update.get("update_id").and_then(Value::as_i64).unwrap_or(0);
            if id >= highest {
                highest = id + 1;
            }
            // Reuse the adapter's normalizer so webhook and poll paths agree.
            if let Ok(Some(event)) = crate::channels::telegram::parse_event(update) {
                events.push(event);
            }
        }
        (events, highest)
    }

    /// True when the outbound queue is empty (used for backoff decisions).
    fn pending_none(outbound: &Mutex<Vec<OutboundMessage>>) -> bool {
        outbound.lock().map(|q| q.is_empty()).unwrap_or(true)
    }

    /// Observer called before the daemon dispatches an inbound event to
    /// the sink: the `pre_gateway_dispatch` fire point. The gateway crate
    /// deliberately depends only on `pantheon-api` + `pantheon-storage`
    /// and so cannot reach the extension manager itself; hosts that own a
    /// manager (the TUI gateway) pass a closure that fires the hook there.
    /// Observer-class by contract: the return value is ignored and a
    /// panicking hook must never stop the dispatch that follows it.
    pub type PreDispatchHook = Arc<dyn Fn(&ChannelEvent) + Send + Sync>;

    /// Route one channel event to the sink. Returns the thread id it came from.
    pub fn route_event(sink: &dyn EventSink, event: &ChannelEvent) -> String {
        route_event_with_hook(sink, event, None)
    }

    /// Like [`route_event`], but fires `hook` (when present) before the
    /// sink sees the event. A hook panic is caught and logged: dispatch
    /// proceeds, because an observer must never veto delivery by crashing.
    pub fn route_event_with_hook(
        sink: &dyn EventSink,
        event: &ChannelEvent,
        hook: Option<&PreDispatchHook>,
    ) -> String {
        if let Some(hook) = hook {
            let hook = Arc::clone(hook);
            let event = event.clone();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                hook(&event);
            }))
            .is_err()
            {
                eprintln!("gateway: pre-dispatch hook panicked; dispatching anyway");
            }
        }
        route_event_inner(sink, event)
    }

    fn route_event_inner(sink: &dyn EventSink, event: &ChannelEvent) -> String {
        match (&event.approval, &event.scope) {
            (Some(answer), Some(scope)) => {
                sink.on_approval(
                    &event.thread_id,
                    event.sender.as_deref(),
                    event.run_id.as_deref(),
                    scope,
                    matches!(answer, crate::channels::channel::ApprovalAnswer::Grant),
                );
                event.thread_id.clone()
            }
            _ => {
                if !event.text.trim().is_empty() {
                    sink.on_message(&event.thread_id, event.sender.as_deref(), &event.text);
                }
                event.thread_id.clone()
            }
        }
    }

    /// Choose the channel that should deliver `msg`. Pure: unit-tested.
    ///
    /// 1. The channel that claimed the thread (polled an event from it) owns
    ///    the reply - first claim wins, so a thread is never stolen.
    /// 2. Otherwise the message's gateway tag names a channel
    ///    ("telegram"/"discord").
    /// 3. Otherwise a lone channel takes it (single-surface daemons).
    /// 4. Otherwise `None`: delivering a reply to the wrong surface is worse
    ///    than dropping it, so the caller dead-letters with a log.
    pub fn route_outbound(
        channels: &[Arc<dyn Channel>],
        claimed: &HashMap<String, String>,
        msg: &OutboundMessage,
    ) -> Option<Arc<dyn Channel>> {
        if let Some(owner) = claimed.get(&msg.to_conversation) {
            if let Some(c) = channels.iter().find(|c| c.name() == owner) {
                return Some(c.clone());
            }
        }
        if !msg.gateway.is_empty() {
            if let Some(c) = channels.iter().find(|c| c.name() == msg.gateway) {
                return Some(c.clone());
            }
        }
        if channels.len() == 1 {
            return Some(channels[0].clone());
        }
        None
    }

    /// What to do with a message whose send just failed: bump the attempt
    /// counter and requeue, or dead-letter once the bound is hit. Pure apart
    /// from the log line, so the retry bound is unit-testable without sleeping
    /// through daemon ticks.
    fn retry_or_dead_letter(
        mut msg: OutboundMessage,
        channel_name: &str,
        err: &ChannelError,
    ) -> Option<OutboundMessage> {
        msg.attempts += 1;
        if msg.attempts >= MAX_SEND_ATTEMPTS {
            eprintln!(
                "daemon: dead-lettering message to '{}' after {} attempts ({}): {err}",
                msg.to_conversation, msg.attempts, channel_name,
            );
            None
        } else {
            eprintln!(
                "daemon: deliver to {channel_name} failed (attempt {}/{MAX_SEND_ATTEMPTS}): {err}",
                msg.attempts,
            );
            Some(msg)
        }
    }

    /// The daemon loop. Runs until `stop` returns true. Each iteration polls
    /// Telegram (long poll), drains any bridge-fed inboxes (Discord via
    /// `push_inbound`), routes events, and flushes outbound messages.
    pub struct ChannelDaemon {
        pub poll_interval: Duration,
        pub long_poll_timeout: u64,
        state_path: PathBuf,
        /// Heartbeat the loop touches each iteration, watched by the
        /// hung-gateway watchdog ([`crate::watchdog`]). `None` = unwatched
        /// (unit tests, embedding callers that supervise differently).
        heartbeat: Option<std::sync::Arc<crate::runtime::watchdog::GatewayHeartbeat>>,
        pre_dispatch: Option<PreDispatchHook>,
    }

    impl ChannelDaemon {
        pub fn new(state_path: PathBuf) -> Self {
            Self {
                poll_interval: Duration::from_secs(1),
                long_poll_timeout: 25,
                state_path,
                heartbeat: None,
                pre_dispatch: None,
            }
        }

        /// Attach a heartbeat for the hung-gateway watchdog. The loop
        /// touches it at the top of every iteration, so a thread blocked
        /// inside a transport call stops ticking and the supervisor can
        /// tell a hung poller from a merely idle one.
        pub fn with_heartbeat(
            mut self,
            heartbeat: std::sync::Arc<crate::runtime::watchdog::GatewayHeartbeat>,
        ) -> Self {
            self.heartbeat = Some(heartbeat);
            self
        }

        /// Attach the pre-dispatch observer (see [`PreDispatchHook`]).
        /// Absent = today's behavior: events go straight to the sink.
        pub fn with_pre_dispatch(mut self, hook: PreDispatchHook) -> Self {
            self.pre_dispatch = Some(hook);
            self
        }

        /// Run until `stop` signals. `channels` are drained each tick (their
        /// `poll()` includes bridge-fed inboxes); `telegram` participates in
        /// true long polling when configured. Outbound delivery is
        /// thread-first (the channel that claimed the thread, else the
        /// message's gateway tag, else a lone channel - never fan-out, never
        /// a wrong surface) and failed sends are retried at most
        /// [`MAX_SEND_ATTEMPTS`] times before the message is dead-lettered
        /// with a log. Rate-limited sends stay queued for the next tick and
        /// honor the platform's `Retry-After` hint.
        pub fn run(
            &self,
            channels: Vec<Arc<dyn Channel>>,
            telegram: Option<(&Arc<TelegramChannel>, &Arc<TelegramRestTransport>)>,
            sink: &dyn EventSink,
            outbound: &Mutex<Vec<OutboundMessage>>,
            stop: &dyn Fn() -> bool,
        ) {
            let cursor = UpdateCursor::new(self.state_path.clone());
            // Channel health (F-7): snapshots live next to the cursor file, in
            // `<data_dir>/gateway/`.
            let state_dir = self
                .state_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            // Retention prunes under the data dir; state_dir is
            // `<data_dir>/gateway/`, so its parent is the data dir.
            let data_dir = state_dir.parent().map(|p| p.to_path_buf());
            let mut health = ChannelHealthTracker::new(state_dir);
            let mut backoff = 0u32;
            // Longest server-asked wait outstanding (from `Retry-After` /
            // `parameters.retry_after` on a 429). The platform knows its window;
            // our exponential guess does not override it.
            let mut rate_wait_ms = 0u64;
            // thread_id -> owning channel name. First claim wins.
            let mut claimed: HashMap<String, String> = HashMap::new();
            // The long-poll path has no channel object of its own; attribute
            // its events to the bound telegram channel (else the first one).
            let telegram_owner: Option<String> = channels
                .iter()
                .find(|c| c.name() == "telegram")
                .or(channels.first())
                .map(|c| c.name().to_string());
            let telegram_name = telegram_owner
                .clone()
                .unwrap_or_else(|| "telegram".to_string());
            while !stop() {
                // Hung-gateway watchdog: tick once per loop iteration, so a
                // thread blocked inside a transport call stops ticking and
                // the supervisor can abandon + restart it (once).
                if let Some(hb) = &self.heartbeat {
                    hb.touch();
                }
                // Retention: prune old events/search chunks/claims past the
                // configured window. The 24h gate lives inside
                // `maybe_run_retention`, so this is a cheap check per tick.
                if let Some(data_dir) = data_dir.as_deref() {
                    pantheon_scheduler::retention::maybe_run_retention(data_dir);
                }
                let mut progressed = false;
                // Telegram long poll (with the persisted cursor). Voice-aware:
                // voice/audio messages are downloaded and transcribed with
                // the telegram channel's voice pipes; declines ride the
                // normal outbound queue below.
                if let Some((channel, transport)) = telegram {
                    match channel.poll_updates(
                        transport.as_ref(),
                        cursor.get(),
                        self.long_poll_timeout,
                    ) {
                        Ok((outcomes, next)) => {
                            health.record_poll_ok(&telegram_name);
                            for outcome in &outcomes {
                                match outcome {
                                    VoiceOutcome::Event(event) => {
                                        route_event_with_hook(
                                            sink,
                                            event,
                                            self.pre_dispatch.as_ref(),
                                        );
                                        if let Some(owner) = &telegram_owner {
                                            claimed
                                                .entry(event.thread_id.clone())
                                                .or_insert_with(|| owner.clone());
                                        }
                                    }
                                    VoiceOutcome::Reply { thread_id, text } => {
                                        let mut out =
                                            outbound.lock().unwrap_or_else(|e| e.into_inner());
                                        out.push(OutboundMessage::new(thread_id, text, "telegram"));
                                    }
                                }
                            }
                            if next > cursor.get() {
                                cursor.advance(next);
                            }
                            if !outcomes.is_empty() {
                                progressed = true;
                            }
                        }
                        Err(e) => {
                            health.record_poll_err(&telegram_name, &e);
                            eprintln!("telegram poll error: {e}");
                        }
                    }
                }
                // Drain every channel's inbox (webhook-fed Discord, bridges).
                for channel in &channels {
                    for event in channel.poll() {
                        route_event_with_hook(sink, &event, self.pre_dispatch.as_ref());
                        claimed
                            .entry(event.thread_id.clone())
                            .or_insert_with(|| channel.name().to_string());
                        progressed = true;
                    }
                }
                // Deliver outbound messages. Never fan-out: one reply goes to
                // one surface, chosen by `route_outbound`. Failed sends are
                // requeued at the front in order so conversations keep
                // sequence; rate-limited ones also force the idle backoff so we
                // don't hot-loop the API.
                {
                    let mut out = outbound.lock().unwrap_or_else(|e| e.into_inner());
                    let msgs: Vec<OutboundMessage> = std::mem::take(&mut *out);
                    drop(out);
                    let mut pending: Vec<OutboundMessage> = Vec::new();
                    let mut rate_limited = false;
                    for msg in msgs {
                        let target = route_outbound(&channels, &claimed, &msg);
                        let Some(channel) = target else {
                            // No owning surface: misdelivering is worse than
                            // dropping, so this is a dead letter with a log.
                            eprintln!(
                                "daemon: no channel for thread '{}' (gateway '{}'); dead-lettering",
                                msg.to_conversation, msg.gateway,
                            );
                            continue;
                        };
                        let envelope = ChannelEnvelope {
                            thread_id: msg.to_conversation.clone(),
                            frame: crate::server::stream::UiFrame {
                                id: 0,
                                kind: crate::server::stream::UiFrameKind::Text,
                                run_id: String::new(),
                                thread_id: msg.to_conversation.clone(),
                                name: "delta".into(),
                                text: msg.text.clone(),
                                interrupt: false,
                                genui: None,
                                component: None,
                            },
                        };
                        match channel.send(envelope) {
                            Ok(()) => {
                                health.record_send_ok(channel.name());
                                progressed = true;
                            }
                            Err(e) => {
                                if e.is_rate_limited() {
                                    rate_limited = true;
                                    // Honor the platform's own wait hint, not
                                    // just our backoff guess.
                                    rate_wait_ms = rate_wait_ms.max(
                                        crate::channels::delivery::retry_delay_ms(&e, backoff),
                                    );
                                    health.record_send_rate_limited(channel.name(), &e);
                                } else {
                                    health.record_send_err(channel.name(), &e);
                                }
                                if let Some(retry) = retry_or_dead_letter(msg, channel.name(), &e) {
                                    pending.push(retry);
                                } else {
                                    health.record_dead_letter(channel.name(), &e.to_string());
                                }
                            }
                        }
                    }
                    if !pending.is_empty() {
                        // Requeue at the front in original order.
                        let mut out = outbound.lock().unwrap_or_else(|e| e.into_inner());
                        pending.extend(std::mem::take(&mut *out));
                        *out = pending;
                        if rate_limited {
                            // Don't count a 429 as idle-but-healthy: force the
                            // backoff path below so we sleep before retrying.
                            backoff = backoff.max(1);
                        }
                    }
                }
                if progressed && pending_none(outbound) {
                    backoff = 0;
                    rate_wait_ms = 0;
                } else if !progressed {
                    backoff = backoff.saturating_add(1);
                }
                // Idle backoff between empty polls; capped like delivery. A
                // server-asked wait overrides the guess when it is longer.
                let wait = if backoff > 0 || rate_wait_ms > 0 {
                    Duration::from_millis(
                        crate::channels::delivery::backoff_ms(backoff.min(6)).max(rate_wait_ms),
                    )
                    .min(
                        self.poll_interval
                            .max(Duration::from_secs(5))
                            .max(Duration::from_millis(rate_wait_ms)),
                    )
                } else {
                    self.poll_interval
                };
                std::thread::sleep(wait);
            }
        }
    }

    #[cfg(test)]
    mod health_tests {
        use super::*;

        fn tmpdir(name: &str) -> PathBuf {
            let d = std::env::temp_dir().join(format!("pantheon-health-test-{name}"));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        #[test]
        fn poll_ok_marks_connected() {
            let d = tmpdir("ok");
            let mut t = ChannelHealthTracker::new(d.clone());
            t.record_poll_ok("telegram");
            let h = t.states.get("telegram").expect("telegram state exists");
            assert_eq!(h.state, ChannelState::Connected);
            assert_eq!(h.consecutive_failures, 0);
            // Snapshot file written on the unknown->connected transition.
            assert!(d.join("channel-health-telegram.json").is_file());
        }

        #[test]
        fn failures_degrade_then_die() {
            let d = tmpdir("degrade");
            let mut t = ChannelHealthTracker::new(d);
            for i in 1..HEALTH_DEAD_AFTER {
                t.record_poll_err("telegram", "getUpdates failed [TELEGRAM_HTTP]");
                let h = t.states.get("telegram").expect("telegram state exists");
                assert_eq!(h.state, ChannelState::Degraded);
                assert_eq!(h.consecutive_failures, i);
            }
            t.record_poll_err("telegram", "getUpdates failed [TELEGRAM_HTTP]");
            assert_eq!(
                t.states
                    .get("telegram")
                    .expect("telegram state exists")
                    .state,
                ChannelState::Dead
            );
        }

        #[test]
        fn revoked_token_is_dead_immediately() {
            let d = tmpdir("revoked");
            let mut t = ChannelHealthTracker::new(d);
            t.record_poll_err(
                "telegram",
                "getUpdates failed [TELEGRAM_HTTP]: 401 unauthorized (token revoked?)",
            );
            let h = t.states.get("telegram").expect("telegram state exists");
            assert_eq!(h.state, ChannelState::Dead);
        }

        #[test]
        fn success_resets_failures() {
            let d = tmpdir("reset");
            let mut t = ChannelHealthTracker::new(d);
            t.record_poll_err("telegram", "boom");
            t.record_poll_err("telegram", "boom");
            t.record_poll_ok("telegram");
            let h = t.states.get("telegram").expect("telegram state exists");
            assert_eq!(h.state, ChannelState::Connected);
            assert_eq!(h.consecutive_failures, 0);
            assert!(h.last_error.is_none());
        }

        #[test]
        fn rate_limit_does_not_spend_failure_budget() {
            let d = tmpdir("ratelimit");
            let mut t = ChannelHealthTracker::new(d);
            let err = ChannelError::rate_limited("TELEGRAM_HTTP", "slow down", Some(5));
            for _ in 0..10 {
                t.record_send_rate_limited("telegram", &err);
            }
            let h = t.states.get("telegram").expect("telegram state exists");
            assert_eq!(h.state, ChannelState::Degraded);
            assert_eq!(h.consecutive_failures, 0);
        }

        #[test]
        fn dead_letter_marks_dead() {
            let d = tmpdir("deadletter");
            let mut t = ChannelHealthTracker::new(d);
            t.record_send_ok("discord");
            t.record_dead_letter("discord", "[DISCORD_HTTP] 404");
            let h = t.states.get("discord").expect("discord state exists");
            assert_eq!(h.state, ChannelState::Dead);
            assert!(h
                .last_error
                .as_deref()
                .expect("last error set")
                .contains("dead-lettered"));
        }

        #[test]
        fn reader_merges_and_marks_stale() {
            let data_dir = tmpdir("reader");
            let gw = data_dir.join("gateway");
            std::fs::create_dir_all(&gw).unwrap();
            // Fresh file: written by a tracker just now.
            let mut t = ChannelHealthTracker::new(gw.clone());
            t.record_poll_ok("telegram");
            // Stale file: hand-written with an ancient timestamp.
            std::fs::write(
            gw.join("channel-health-discord.json"),
            r#"{"name":"discord","state":"connected","last_error":null,"last_success_ms":1,"consecutive_failures":0,"updated_ms":1}"#,
        )
        .unwrap();
            let v = channel_health_json(&data_dir);
            let channels = v
                .get("channels")
                .expect("channels present")
                .as_array()
                .expect("channels is array");
            assert_eq!(channels.len(), 2);
            // Sorted by name: discord first.
            assert_eq!(channels[0].get("name").expect("name present"), "discord");
            assert_eq!(
                channels[0].get("stale"),
                Some(&serde_json::Value::Bool(true))
            );
            assert_eq!(channels[1].get("name").expect("name present"), "telegram");
            assert!(channels[1].get("stale").is_none());
        }

        #[test]
        fn reader_empty_without_gateway_dir() {
            let data_dir = tmpdir("reader-empty");
            let v = channel_health_json(&data_dir);
            assert_eq!(
                v.get("channels")
                    .expect("channels present")
                    .as_array()
                    .expect("channels is array")
                    .len(),
                0
            );
        }
    }

    #[cfg(test)]
    mod pre_dispatch_tests {
        use super::*;
        use std::sync::Mutex;

        struct RecSink {
            order: std::sync::Arc<Mutex<Vec<String>>>,
        }
        impl EventSink for RecSink {
            fn on_message(&self, _thread_id: &str, _sender: Option<&str>, _text: &str) {
                self.order.lock().unwrap().push("sink".to_string());
            }
            fn on_approval(
                &self,
                _thread_id: &str,
                _sender: Option<&str>,
                _run_id: Option<&str>,
                _scope: &str,
                _grant: bool,
            ) {
                self.order.lock().unwrap().push("sink".to_string());
            }
        }

        fn event() -> ChannelEvent {
            ChannelEvent {
                thread_id: "t1".to_string(),
                run_id: None,
                text: "hello".to_string(),
                approval: None,
                scope: None,
                sender: Some("user-1".to_string()),
            }
        }

        #[test]
        fn pre_dispatch_hook_fires_before_the_sink_and_a_panicking_hook_does_not_block_dispatch() {
            let order = std::sync::Arc::new(Mutex::new(vec![]));
            let sink = RecSink {
                order: order.clone(),
            };
            let seen = order.clone();
            let hook: PreDispatchHook = std::sync::Arc::new(move |ev: &ChannelEvent| {
                seen.lock().unwrap().push(format!("hook:{}", ev.text));
            });
            let thread = route_event_with_hook(&sink, &event(), Some(&hook));
            assert_eq!(thread, "t1");
            assert_eq!(
                *order.lock().unwrap(),
                vec!["hook:hello".to_string(), "sink".to_string()],
                "hook must fire before the sink dispatches"
            );

            // A panicking observer must never veto delivery: the sink
            // still runs, and the panic is swallowed with a log line.
            let panicking: PreDispatchHook =
                std::sync::Arc::new(|_ev: &ChannelEvent| panic!("boom"));
            let order2 = std::sync::Arc::new(Mutex::new(vec![]));
            let sink2 = RecSink {
                order: order2.clone(),
            };
            let thread = route_event_with_hook(&sink2, &event(), Some(&panicking));
            assert_eq!(thread, "t1");
            assert_eq!(*order2.lock().unwrap(), vec!["sink".to_string()]);
        }
    }
}
pub mod scheduler {
    //! Scheduler loop for the gateway service.
    //!
    //! The gateway background service ticks due scheduled jobs on an interval
    //! using `pantheon-scheduler`'s [`TickDriver`] and [`DurableClaimLedger`].
    //! Job loading and execution are injected as closures so this crate stays
    //! free of runtime/config dependencies - the app crate wires in its job
    //! store and `run_job_now`.
    //!
    //! The loop and `pantheon schedule tick` open the same claim ledger
    //! (`<data_dir>/claims.db`) and the same job store (`<data_dir>/schedule.json`),
    //! so the two can never double-fire an occurrence: the claim is an atomic
    //! first-wins INSERT, and a lost race simply skips.

    use pantheon_api::logging::now_ms;
    use pantheon_scheduler::{
        tick::PausedCheck, DurableClaimLedger, RunOutcome, ScheduleKind, TickDecision, TickDriver,
    };
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    /// One job the loop knows how to fire: the scheduler core's shared shape
    /// the [`pantheon_scheduler::Job`] plus the last fire time driving the due
    /// check. Re-exported here so callers (e.g. the app crate building the
    /// `ExecuteFn`) name one type. Task text is resolved by the app crate via
    /// `job.resolve_task(&TemplateStore::open(data_dir))` before running.
    pub use pantheon_scheduler::ScheduledJob;

    /// How one job fared in a tick pass.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum FireOutcome {
        /// The claim was won and the run started.
        Fired,
        /// The job was still running and its overlap policy queued another run.
        Queued,
        /// Not fired; the human-readable reason is carried along.
        Skipped(String),
    }

    /// One job's result from a tick pass.
    #[derive(Debug, Clone)]
    pub struct TickReport {
        pub id: String,
        pub outcome: FireOutcome,
    }

    /// Job execution, injected by the app crate. Returns the task-level
    /// outcome: the tick driver still reports its own [`RunOutcome`]
    /// (executor returned / panicked / timed out), but a task can fail while
    /// its thread returns normally (e.g. the agent run errored), and the
    /// outcome sink needs that detail for run history and self-heal.
    /// `Arc` (not a borrow) because the tick driver runs jobs on detached
    /// threads, so the closure must be `'static`.
    pub type ExecuteFn = Arc<dyn Fn(&ScheduledJob) -> TaskOutcome + Send + Sync>;

    /// What one fired run's task produced, as opposed to what its thread
    /// did. The driver reports [`RunOutcome`]; this travels alongside so
    /// the outcome sink can record the *task's* result - including the
    /// run id (for ledger alerts) and the failure detail when the task
    /// itself failed.
    #[derive(Debug, Clone, Default)]
    pub struct TaskOutcome {
        /// The job run's id ("" when the job type creates no run, e.g. the
        /// nightly pass, or when the run never started).
        pub run_id: String,
        /// Task-level failure detail. `None` means the task itself
        /// succeeded, even if the driver later reports something else.
        pub error: Option<String>,
    }

    /// Receives one fired run's outcome: called with the job id, the
    /// driver's [`RunOutcome`], and the task-level [`TaskOutcome`] (None
    /// when the executor thread never reported one - panic or timeout
    /// abandon) when the run finishes (or is abandoned on timeout).
    /// Installed via [`SchedulerLoop::set_outcome_sink`]; the run-history
    /// recorder and the self-heal pipeline are the intended consumers.
    /// `None` (the default) keeps the old behavior of dropping the
    /// completion channel.
    pub type OutcomeSink = Arc<dyn Fn(&str, RunOutcome, Option<TaskOutcome>) + Send + Sync>;

    /// Short relative time for status lines: "now", "in 5m", "in 2h 5m",
    /// "in 3d 2h". Pure, so tests assert it without a clock.
    pub fn rel_time(delta_ms: i64) -> String {
        if delta_ms <= 0 {
            return "now".to_string();
        }
        let secs = delta_ms / 1000;
        let days = secs / 86_400;
        let hours = (secs % 86_400) / 3600;
        let mins = (secs % 3600) / 60;
        if days > 0 {
            format!("in {days}d {hours}h")
        } else if hours > 0 {
            format!("in {hours}h {mins}m")
        } else if mins > 0 {
            format!("in {mins}m")
        } else {
            format!("in {secs}s")
        }
    }

    /// One-line scheduler queue summary for `pantheon gateway status` and
    /// `pantheon init`: active jobs, how many are due right now, and the next
    /// scheduled fire. Pure function of the jobs and the clock.
    pub fn queue_summary(jobs: &[ScheduledJob], now_ms: i64) -> String {
        let mut active = 0usize;
        let mut due_now = 0usize;
        let mut next: Option<(&str, i64)> = None;
        for j in jobs {
            if j.job.paused {
                continue;
            }
            active += 1;
            let is_due = j.job.due(now_ms, j.last_run);
            if is_due {
                due_now += 1;
            }
            // A job due right now is already counted above; "next" is the next
            // future fire after the pending one.
            if !is_due {
                if let Some(t) = j.job.next_fire_ms(now_ms, j.last_run) {
                    if next.is_none_or(|(_, nt)| t < nt) {
                        next = Some((j.job.id.as_str(), t));
                    }
                }
            }
        }
        let mut s = format!("{active} active, {due_now} due now");
        match next {
            Some((id, t)) => {
                s.push_str(&format!("; next: {id} {}", rel_time(t - now_ms)));
            }
            None => {
                if active > 0 {
                    s.push_str("; no future fire scheduled");
                }
            }
        }
        s
    }

    /// Owns the tick loop. Share the claim ledger path with every other
    /// ticker; the ledger is the single source of truth.
    pub struct SchedulerLoop {
        driver: Arc<TickDriver>,
        interval: Duration,
        outcome_sink: Option<OutcomeSink>,
        /// Completion observer for one-shot jobs. One-shot rows are removed
        /// only when the run actually completed - deleting on fire would lose
        /// panicked/timed-out runs silently. Kept separate from
        /// [`SchedulerLoop::outcome_sink`] so installing a run-history sink
        /// never disables one-shot lifecycle handling.
        oneshot_sink: Option<OutcomeSink>,
    }

    impl SchedulerLoop {
        /// Open on `<data_dir>/claims.db`. Fails when the ledger cannot be
        /// opened - a tick loop that cannot claim must not fire.
        pub fn open(data_dir: &Path, tick_secs: u64) -> Result<Self, String> {
            let ledger = DurableClaimLedger::open(&data_dir.join("claims.db"))
                .map_err(|e| format!("cannot open claim ledger: {e}"))?;
            Ok(Self {
                driver: Arc::new(TickDriver::new(ledger)),
                interval: Duration::from_secs(tick_secs.max(1)),
                outcome_sink: None,
                oneshot_sink: None,
            })
        }

        /// Install the outcome sink: every fired run's [`RunOutcome`] plus
        /// the task-level [`TaskOutcome`] is delivered to it on a watcher
        /// thread when the run finishes. Opt-in; without it the completion
        /// channel is dropped as before.
        pub fn set_outcome_sink(&mut self, sink: OutcomeSink) {
            self.outcome_sink = Some(sink);
        }

        /// Install the one-shot completion observer: called with the job id,
        /// its [`RunOutcome`], and the task-level [`TaskOutcome`] when a
        /// one-shot run finishes (or is abandoned on timeout). The app crate
        /// uses this to remove one-shot rows only on
        /// [`RunOutcome::Completed`].
        pub fn set_oneshot_sink(&mut self, sink: OutcomeSink) {
            self.oneshot_sink = Some(sink);
        }

        /// Install the paused-state predicate the tick driver consults before
        /// draining a queued fire (see [`TickDriver::set_paused_check`]).
        pub fn set_paused_check(&self, check: PausedCheck) {
            self.driver.set_paused_check(check);
        }

        /// One pass over `jobs`: fire everything due. Returns a report per
        /// job that was due, plus the completion-watcher thread handles:
        /// each watcher delivers one fired run's outcome to the sinks when
        /// the run settles. The daemon loop drops the handles (watchers are
        /// detached, as before); `pantheon schedule tick` (one-shot) joins
        /// them so run history and self-heal complete before the process
        /// exits.
        pub fn tick_once(
            &self,
            now_ms: i64,
            jobs: &[ScheduledJob],
            execute: ExecuteFn,
        ) -> (Vec<TickReport>, Vec<std::thread::JoinHandle<()>>) {
            let mut out = Vec::new();
            let mut watchers = Vec::new();
            for j in jobs.iter().filter(|j| !j.job.paused) {
                let job = j.clone();
                let ex = execute.clone();
                // Per-fire slot for the task-level outcome. The thunk stores
                // it; the watcher drains it. A panicked or timed-out run
                // never fills the slot, so the sink sees None - and a late
                // fill from a detached (abandoned) run lands in a dropped
                // slot, never in a later fire. No cross-fire leakage.
                let slot: Arc<std::sync::Mutex<Option<TaskOutcome>>> =
                    Arc::new(std::sync::Mutex::new(None));
                let slot_w = Arc::clone(&slot);
                let thunk: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
                    let r = ex(&job);
                    if let Ok(mut g) = slot_w.lock() {
                        *g = Some(r);
                    }
                });
                let outcome = match self.driver.tick_job(&j.job, now_ms, j.last_run, thunk) {
                    TickDecision::Fired { completion } => {
                        // Completion observation fans out on one watcher
                        // thread, which owns the receiver: the outcome sink
                        // (run history + self-heal) gets every outcome, and
                        // the one-shot sink gets one-shot outcomes so
                        // one-shot rows are removed only when the run
                        // actually finished. With neither installed the
                        // channel is dropped as before.
                        let is_oneshot = matches!(j.job.kind, ScheduleKind::OneShot { .. });
                        if self.outcome_sink.is_some()
                            || (is_oneshot && self.oneshot_sink.is_some())
                        {
                            let id = j.job.id.clone();
                            let sink = self.outcome_sink.clone();
                            let osink = self.oneshot_sink.clone();
                            watchers.push(std::thread::spawn(move || {
                                if let Ok(outcome) = completion.recv() {
                                    let task = slot.lock().ok().and_then(|mut g| g.take());
                                    if is_oneshot {
                                        if let Some(os) = osink {
                                            os(&id, outcome, task.clone());
                                        }
                                    }
                                    if let Some(s) = sink {
                                        s(&id, outcome, task);
                                    }
                                }
                            }));
                        }
                        FireOutcome::Fired
                    }
                    TickDecision::Queued => FireOutcome::Queued,
                    TickDecision::NotDue => continue,
                    TickDecision::SkippedClaimLost => {
                        FireOutcome::Skipped("already claimed, skipping (replay)".to_string())
                    }
                    TickDecision::SkippedOverlap => {
                        FireOutcome::Skipped("still running, skipping (overlap=skip)".to_string())
                    }
                    TickDecision::ClaimFailed(e) => {
                        FireOutcome::Skipped(format!("claim failed, not run: {e}"))
                    }
                };
                out.push(TickReport {
                    id: j.job.id.clone(),
                    outcome,
                });
            }
            (out, watchers)
        }

        /// Loop forever: reload jobs every pass (so `schedule` edits apply
        /// without restarting the service), tick, report, sleep.
        pub fn run_forever(
            &self,
            load: &(dyn Fn() -> Vec<ScheduledJob> + Send + Sync),
            execute: ExecuteFn,
            on_tick: &(dyn Fn(&[TickReport], i64) + Send + Sync),
        ) -> ! {
            loop {
                let now = now_ms();
                let jobs = load();
                let (reports, _watchers) = self.tick_once(now, &jobs, execute.clone());
                // Daemon loop: watchers are detached; outcomes reach the
                // sinks on their own threads, same as before.
                on_tick(&reports, now);
                std::thread::sleep(self.interval);
            }
        }
    }
}
pub mod service {
    //! Silent user-scope service install for the gateway.
    //!
    //! Some harnesses install their gateway service unconditionally at setup,
    //! in user scope, without prompting - "always safe to call". This module is
    //! Pantheon's equivalent. [`install`] detects the platform's service
    //! manager at runtime and installs accordingly:
    //!
    //! - Linux + systemd → user unit `~/.config/systemd/user/pantheon-gateway.service`
    //!   + `systemctl --user enable --now` (falls back to cron when the user
    //!     bus is unavailable, e.g. containers)
    //! - Linux without systemd → `crontab` `@reboot` entry, merged idempotently
    //! - macOS → LaunchAgent plist under `~/Library/LaunchAgents` + `launchctl bootstrap`
    //! - Windows → Task Scheduler logon task via `schtasks /Create` (user
    //!   scope, no admin; `/F` makes re-runs idempotent)
    //! - anything else → fail open with a one-line note showing the manual
    //!   `pantheon schedule tick` cron line
    //!
    //! Re-running install when already installed is a no-op that ensures the
    //! service is enabled and running. Nothing here prompts; nothing here
    //! panics on a missing manager.

    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    /// systemd unit name / launchd-agnostic service name.
    pub const UNIT_NAME: &str = "pantheon-gateway";
    /// macOS LaunchAgent label.
    pub const LAUNCHD_LABEL: &str = "africa.exoseed.pantheon.gateway";
    /// Windows Task Scheduler task name.
    pub const TASK_NAME: &str = "PantheonGateway";
    /// Marker appended to the cron line we own, so the merge can find and
    /// replace it idempotently without touching the user's other entries.
    pub const CRON_MARKER: &str = "# pantheon-gateway";

    /// Which background mechanism this platform uses.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mechanism {
        Systemd,
        Cron,
        Launchd,
        TaskScheduler,
        None,
    }

    impl Mechanism {
        pub fn as_str(&self) -> &'static str {
            match self {
                Mechanism::Systemd => "systemd",
                Mechanism::Cron => "cron",
                Mechanism::Launchd => "launchd",
                Mechanism::TaskScheduler => "task-scheduler",
                Mechanism::None => "none",
            }
        }
    }

    /// Injectable host environment so tests can redirect unit dirs and PATH
    /// without touching the real system.
    #[derive(Debug, Clone)]
    pub struct InstallEnv {
        /// Directories searched for helper binaries, in order. Real: `PATH`.
        pub path_dirs: Vec<PathBuf>,
        /// systemd user unit dir. Real: `$XDG_CONFIG_HOME/systemd/user` or
        /// `~/.config/systemd/user`. Honors `SYSTEMD_USER_CONFIG_DIR`.
        pub systemd_user_dir: PathBuf,
        /// LaunchAgents dir. Real: `~/Library/LaunchAgents`. Honors
        /// `PANTHEON_LAUNCHD_DIR` (test hook).
        pub launchd_agents_dir: PathBuf,
    }

    impl InstallEnv {
        pub fn real() -> Self {
            let path_dirs = std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default();
            Self {
                path_dirs,
                systemd_user_dir: systemd_user_dir(),
                launchd_agents_dir: launchd_agents_dir(),
            }
        }

        fn which(&self, bin: &str) -> Option<PathBuf> {
            // On Windows the binary is schtasks.exe; PATH lookup needs the suffix.
            let candidates: Vec<String> = if cfg!(target_os = "windows") && !bin.contains('.') {
                vec![format!("{bin}.exe"), bin.to_string()]
            } else {
                vec![bin.to_string()]
            };
            for dir in &self.path_dirs {
                for name in &candidates {
                    let cand = dir.join(name);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
            }
            None
        }
    }

    fn systemd_user_dir() -> PathBuf {
        if let Some(d) = std::env::var_os("SYSTEMD_USER_CONFIG_DIR") {
            return PathBuf::from(d);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            });
        base.join("systemd").join("user")
    }

    fn launchd_agents_dir() -> PathBuf {
        if let Some(d) = std::env::var_os("PANTHEON_LAUNCHD_DIR") {
            return PathBuf::from(d);
        }
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("Library")
            .join("LaunchAgents")
    }

    /// Detect the background mechanism for this platform at runtime.
    pub fn detect(env: &InstallEnv) -> Mechanism {
        if cfg!(target_os = "windows") {
            return if env.which("schtasks").is_some() {
                Mechanism::TaskScheduler
            } else {
                Mechanism::None
            };
        }
        if cfg!(target_os = "macos") {
            return if env.which("launchctl").is_some() {
                Mechanism::Launchd
            } else {
                Mechanism::None
            };
        }
        if cfg!(target_os = "linux") {
            if env.which("systemctl").is_some() {
                return Mechanism::Systemd;
            }
            if env.which("crontab").is_some() {
                return Mechanism::Cron;
            }
            return Mechanism::None;
        }
        Mechanism::None
    }

    /// Absolute path to the running binary, resolved from argv[0] with a
    /// PATH lookup as fallback. systemd/schtasks need an absolute ExecStart;
    /// a bare `pantheon` would resolve against the service's own PATH and can
    /// differ from the shell the user typed the command in.
    pub fn self_exe() -> Option<PathBuf> {
        if let Ok(p) = std::env::current_exe() {
            if p.is_absolute() && p.exists() {
                return Some(p);
            }
        }
        let name = std::env::args().next()?;
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let cand = dir.join(&name);
            if cand.is_file() {
                return Some(cand);
            }
        }
        None
    }

    /// Render the systemd user unit. Pure: the file content is a pure function
    /// of the binary and data dir, which is what makes install idempotent.
    pub fn render_systemd_unit(exe: &Path, data_dir: &Path) -> String {
        format!(
            "[Unit]\n\
         Description=Pantheon gateway (chat surfaces + scheduled tasks)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} gateway run\n\
         Environment=PANTHEON_DATA_DIR={data_dir}\n\
         Restart=always\n\
         RestartSec=5\n\
         # The gateway executes whatever a message asks for on this host.\n\
         # The allowlist in .env is the real control; this is defence in depth.\n\
         NoNewPrivileges=true\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
            exe = exe.display(),
            data_dir = data_dir.display(),
        )
    }

    /// Render the macOS LaunchAgent plist. Pure.
    pub fn render_launchd_plist(exe: &Path, data_dir: &Path) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \n\
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key><string>{LAUNCHD_LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array><string>{exe}</string><string>gateway</string><string>run</string></array>\n\
         \x20 <key>EnvironmentVariables</key>\n\
         \x20 <dict><key>PANTHEON_DATA_DIR</key><string>{data_dir}</string></dict>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         </dict>\n\
         </plist>\n",
            exe = exe.display(),
            data_dir = data_dir.display(),
        )
    }

    fn xml_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                _ => out.push(c),
            }
        }
        out
    }

    /// Render the Windows Task Scheduler task XML (Task Scheduler 2.0 schema).
    /// Logon trigger, least-privilege principal, no admin required. Pure.
    pub fn render_task_xml(exe: &Path, data_dir: &Path) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         \x20 <RegistrationInfo>\n\
         \x20\x20 <Description>Pantheon gateway (chat surfaces + scheduled tasks)</Description>\n\
         \x20 </RegistrationInfo>\n\
         \x20 <Triggers>\n\
         \x20\x20 <LogonTrigger>\n\
         \x20\x20\x20 <Enabled>true</Enabled>\n\
         \x20\x20 </LogonTrigger>\n\
         \x20 </Triggers>\n\
         \x20 <Principals>\n\
         \x20\x20 <Principal id=\"Author\">\n\
         \x20\x20\x20 <LogonType>InteractiveToken</LogonType>\n\
         \x20\x20\x20 <RunLevel>LeastPrivilege</RunLevel>\n\
         \x20\x20 </Principal>\n\
         \x20 </Principals>\n\
         \x20 <Settings>\n\
         \x20\x20 <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
         \x20\x20 <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
         \x20\x20 <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
         \x20\x20 <AllowHardTerminate>true</AllowHardTerminate>\n\
         \x20\x20 <StartWhenAvailable>true</StartWhenAvailable>\n\
         \x20\x20 <Enabled>true</Enabled>\n\
         \x20 </Settings>\n\
         \x20 <Actions Context=\"Author\">\n\
         \x20\x20 <Exec>\n\
         \x20\x20\x20 <Command>{exe}</Command>\n\
         \x20\x20\x20 <Arguments>gateway run</Arguments>\n\
         \x20\x20\x20 <WorkingDirectory>{data_dir}</WorkingDirectory>\n\
         \x20\x20 </Exec>\n\
         \x20 </Actions>\n\
         </Task>\n",
            exe = xml_escape(&exe.display().to_string()),
            data_dir = xml_escape(&data_dir.display().to_string()),
        )
    }

    /// The cron line we own: `@reboot` starts the gateway (scheduler included)
    /// when the machine boots. The marker makes the merge idempotent.
    pub fn cron_line(exe: &Path) -> String {
        format!("@reboot {} gateway run {CRON_MARKER}", exe.display())
    }

    /// Merge our cron line into an existing crontab. Idempotent: any previous
    /// line carrying our marker is removed first, so installing twice yields
    /// byte-identical output. The user's other entries are untouched.
    pub fn merge_crontab(existing: &str, line: &str) -> String {
        let mut kept: Vec<&str> = existing
            .lines()
            .filter(|l| !l.contains(CRON_MARKER))
            .collect();
        kept.push(line);
        let mut out = kept.join("\n");
        out.push('\n');
        out
    }

    /// Manual fallback line shown when no service manager exists: a plain cron
    /// entry that fires due jobs every five minutes.
    pub fn manual_cron_line(exe: &Path) -> String {
        format!("*/5 * * * * {} schedule tick {CRON_MARKER}", exe.display())
    }

    fn run_cmd(env: &InstallEnv, bin: &str, args: &[&str]) -> Result<(), String> {
        let abs = env
            .which(bin)
            .ok_or_else(|| format!("{bin} not found on PATH"))?;
        let status = Command::new(&abs)
            .args(args)
            .status()
            .map_err(|e| format!("run {bin}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{bin} {} failed: {status}", args.join(" ")))
        }
    }

    /// Why a captured command did not produce output. Callers that treat a
    /// failed read as "empty result" (`crontab -l` on a machine with no
    /// crontab yet) must NOT also swallow `Unavailable`: a missing binary, a
    /// spawn failure or an I/O error is not an empty table, and acting on it
    /// as if it were would overwrite real state.
    enum CaptureFailure {
        /// The binary ran and exited non-zero (e.g. `crontab -l` with no
        /// crontab: exit 1 means "empty", not "broken").
        Exited(String),
        /// The binary could not be executed at all.
        Unavailable(String),
    }

    impl From<CaptureFailure> for String {
        fn from(f: CaptureFailure) -> Self {
            match f {
                CaptureFailure::Exited(m) | CaptureFailure::Unavailable(m) => m,
            }
        }
    }

    fn run_capture(env: &InstallEnv, bin: &str, args: &[&str]) -> Result<String, CaptureFailure> {
        let abs = env
            .which(bin)
            .ok_or_else(|| CaptureFailure::Unavailable(format!("{bin} not found on PATH")))?;
        let out = Command::new(&abs)
            .args(args)
            .output()
            .map_err(|e| CaptureFailure::Unavailable(format!("run {bin}: {e}")))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(CaptureFailure::Exited(format!(
                "{bin} {} failed: {}",
                args.join(" "),
                out.status
            )))
        }
    }

    fn install_systemd(env: &InstallEnv, data_dir: &Path, exe: &Path) -> Result<bool, String> {
        let dir = &env.systemd_user_dir;
        let path = dir.join(format!("{UNIT_NAME}.service"));
        let body = render_systemd_unit(exe, data_dir);
        let changed = std::fs::read_to_string(&path)
            .map(|old| old != body)
            .unwrap_or(true);
        if changed {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
            std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
            // daemon-reload picks up a unit that did not exist when the
            // manager last read its directory.
            run_cmd(env, "systemctl", &["--user", "daemon-reload"])?;
        }
        // enable --now is idempotent: re-running only ensures enabled+running.
        run_cmd(env, "systemctl", &["--user", "enable", "--now", UNIT_NAME])?;
        Ok(changed)
    }

    fn current_uid(env: &InstallEnv) -> Result<String, String> {
        let out = run_capture(env, "id", &["-u"])?;
        let uid = out.trim().to_string();
        if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("unexpected `id -u` output: {uid:?}"));
        }
        Ok(uid)
    }

    fn install_launchd(env: &InstallEnv, data_dir: &Path, exe: &Path) -> Result<bool, String> {
        let dir = &env.launchd_agents_dir;
        let path = dir.join(format!("{LAUNCHD_LABEL}.plist"));
        let body = render_launchd_plist(exe, data_dir);
        let changed = std::fs::read_to_string(&path)
            .map(|old| old != body)
            .unwrap_or(true);
        if changed {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
            std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        let uid = current_uid(env)?;
        let domain = format!("gui/{uid}");
        // bootout first (ignored when not loaded) so re-install is idempotent:
        // bootstrap refuses a label that is already bootstrapped.
        let _ = run_cmd(
            env,
            "launchctl",
            &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
        );
        run_cmd(
            env,
            "launchctl",
            &["bootstrap", &domain, &path.to_string_lossy()],
        )?;
        Ok(changed)
    }

    fn install_cron(env: &InstallEnv, exe: &Path) -> Result<bool, String> {
        let line = cron_line(exe);
        // Read the current crontab. `crontab -l` exits non-zero when the user
        // has no crontab yet, which means an *empty table* (nothing to merge
        // with) - but a binary that cannot be executed at all must NOT be read
        // as empty: writing our line alone would silently drop every entry the
        // user already had.
        let current = match run_capture(env, "crontab", &["-l"]) {
            Ok(out) => out,
            Err(CaptureFailure::Exited(_)) => String::new(),
            Err(CaptureFailure::Unavailable(e)) => return Err(e),
        };
        let merged = merge_crontab(&current, &line);
        if merged.trim_end() == current.trim_end() {
            return Ok(false);
        }
        let abs = env.which("crontab").ok_or("crontab not found on PATH")?;
        let mut child = Command::new(&abs)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("run crontab: {e}"))?;
        let status = child
            .stdin
            .as_mut()
            .ok_or("crontab stdin unavailable")?
            .write_all(merged.as_bytes())
            .map_err(|e| format!("write crontab: {e}"))
            .and_then(|()| child.wait().map_err(|e| format!("wait crontab: {e}")))?;
        if status.success() {
            Ok(true)
        } else {
            Err(format!("crontab - failed: {status}"))
        }
    }

    fn install_task_scheduler(
        env: &InstallEnv,
        data_dir: &Path,
        exe: &Path,
    ) -> Result<bool, String> {
        let xml = render_task_xml(exe, data_dir);
        let tmp = std::env::temp_dir().join("pantheon-gateway-task.xml");
        std::fs::write(&tmp, xml).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        // /F overwrites an existing task, so re-running converges.
        let result = run_cmd(
            env,
            "schtasks",
            &[
                "/Create",
                "/TN",
                TASK_NAME,
                "/XML",
                &tmp.to_string_lossy(),
                "/F",
            ],
        );
        let _ = std::fs::remove_file(&tmp);
        result.map(|()| true)
    }

    /// What [`install`] did.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum InstallOutcome {
        /// Installed (or already installed) and ensured running.
        Installed {
            mechanism: Mechanism,
            /// False when nothing changed: the previous install was already current.
            changed: bool,
        },
        /// No service manager on this platform. The note is the one-line
        /// fail-open message for the user.
        Unavailable { note: String },
        /// A manager was found but the install failed.
        Failed { mechanism: Mechanism, error: String },
    }

    /// Install and start the gateway as a user background service.
    ///
    /// Idempotent, never prompts, fails open: always safe to call. On Linux,
    /// a `systemctl` without a working user bus (containers) falls back to
    /// the cron `@reboot` entry automatically.
    pub fn install(data_dir: &Path, exe: &Path) -> InstallOutcome {
        install_with(&InstallEnv::real(), data_dir, exe)
    }

    /// [`install`] with an injectable environment (tests).
    pub fn install_with(env: &InstallEnv, data_dir: &Path, exe: &Path) -> InstallOutcome {
        match detect(env) {
            Mechanism::Systemd => match install_systemd(env, data_dir, exe) {
                Ok(changed) => InstallOutcome::Installed {
                    mechanism: Mechanism::Systemd,
                    changed,
                },
                Err(_) if env.which("crontab").is_some() => match install_cron(env, exe) {
                    Ok(changed) => InstallOutcome::Installed {
                        mechanism: Mechanism::Cron,
                        changed,
                    },
                    Err(e2) => InstallOutcome::Failed {
                        mechanism: Mechanism::Cron,
                        error: e2,
                    },
                },
                Err(e) => InstallOutcome::Failed {
                    mechanism: Mechanism::Systemd,
                    error: e,
                },
            },
            Mechanism::Cron => match install_cron(env, exe) {
                Ok(changed) => InstallOutcome::Installed {
                    mechanism: Mechanism::Cron,
                    changed,
                },
                Err(e) => InstallOutcome::Failed {
                    mechanism: Mechanism::Cron,
                    error: e,
                },
            },
            Mechanism::Launchd => match install_launchd(env, data_dir, exe) {
                Ok(changed) => InstallOutcome::Installed {
                    mechanism: Mechanism::Launchd,
                    changed,
                },
                Err(e) => InstallOutcome::Failed {
                    mechanism: Mechanism::Launchd,
                    error: e,
                },
            },
            Mechanism::TaskScheduler => match install_task_scheduler(env, data_dir, exe) {
                Ok(changed) => InstallOutcome::Installed {
                    mechanism: Mechanism::TaskScheduler,
                    changed,
                },
                Err(e) => InstallOutcome::Failed {
                    mechanism: Mechanism::TaskScheduler,
                    error: e,
                },
            },
            Mechanism::None => InstallOutcome::Unavailable {
                note: format!(
                    "no service manager found; for scheduled tasks add to cron: {}",
                    manual_cron_line(exe)
                ),
            },
        }
    }

    /// Point-in-time service state.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ServiceStatus {
        /// What this platform would use for a fresh install.
        pub detected: Mechanism,
        /// The mechanism actually installed, if any.
        pub installed: Option<Mechanism>,
        /// Whether it is currently running (cron counts as running: cron owns it).
        pub running: bool,
    }

    /// Inspect the service without changing anything: status never installs.
    pub fn status() -> ServiceStatus {
        status_with(&InstallEnv::real())
    }

    /// [`status`] with an injectable environment (tests).
    pub fn status_with(env: &InstallEnv) -> ServiceStatus {
        let detected = detect(env);
        // systemd user unit
        if env
            .systemd_user_dir
            .join(format!("{UNIT_NAME}.service"))
            .exists()
        {
            let running = run_capture(env, "systemctl", &["--user", "is-active", UNIT_NAME])
                .map(|s| s.trim() == "active")
                .unwrap_or(false);
            return ServiceStatus {
                detected,
                installed: Some(Mechanism::Systemd),
                running,
            };
        }
        // launchd plist
        if env
            .launchd_agents_dir
            .join(format!("{LAUNCHD_LABEL}.plist"))
            .exists()
        {
            let running = current_uid(env)
                .ok()
                .and_then(|uid| {
                    run_capture(
                        env,
                        "launchctl",
                        &["print", &format!("gui/{uid}/{LAUNCHD_LABEL}")],
                    )
                    .ok()
                })
                .is_some_and(|out| out.contains("state = running"));
            return ServiceStatus {
                detected,
                installed: Some(Mechanism::Launchd),
                running,
            };
        }
        // cron @reboot entry
        if let Ok(cur) = run_capture(env, "crontab", &["-l"]) {
            if cur.lines().any(|l| l.contains(CRON_MARKER)) {
                return ServiceStatus {
                    detected,
                    installed: Some(Mechanism::Cron),
                    running: true,
                };
            }
        }
        // windows task scheduler
        if let Ok(out) = run_capture(
            env,
            "schtasks",
            &["/Query", "/TN", TASK_NAME, "/FO", "LIST"],
        ) {
            // schtasks /Query /FO LIST prints "Status: Running" for a live task.
            let running = out.lines().any(|l| {
                let l = l.trim_start();
                l.starts_with("Status:") && l.contains("Running")
            });
            return ServiceStatus {
                detected,
                installed: Some(Mechanism::TaskScheduler),
                running,
            };
        }
        ServiceStatus {
            detected,
            installed: None,
            running: false,
        }
    }

    // ── gateway run startup ─────────────────────────────────────────────────
    // `gateway run` is both the chat gateway and the always-on scheduler. The
    // scheduler loop starts unconditionally - a scheduler-only install (no bot
    // tokens) is the main always-on use case - while each chat surface starts
    // only when its token and the allowlist are present. The old behavior was
    // exit(2) when anything was missing, which turned a scheduler-only service
    // into a crash-loop under the service manager.

    /// Which chat surfaces `gateway run` should start, decided purely from env
    /// values. Pure: no process I/O, no exits - testable without spawning
    /// anything. The scheduler loop is deliberately NOT part of this plan: it
    /// always starts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ChannelPlan {
        pub telegram: bool,
        pub discord: bool,
    }

    impl ChannelPlan {
        /// Decide from raw env values. A surface starts only when its token is
        /// present AND the allowlist is non-empty: a token without an allowlist
        /// would hand the host to anyone who finds the bot.
        pub fn from_env(
            discord_token: Option<&str>,
            telegram_token: Option<&str>,
            allow: &std::collections::HashSet<String>,
        ) -> Self {
            let token_ok = |t: Option<&str>| t.is_some_and(|s| !s.trim().is_empty());
            let gated = !allow.is_empty();
            Self {
                telegram: token_ok(telegram_token) && gated,
                discord: token_ok(discord_token) && gated,
            }
        }

        /// True when no chat surface starts: scheduler-only mode.
        pub fn is_empty(&self) -> bool {
            !self.telegram && !self.discord
        }
    }

    /// Read the channel tokens from the secrets store, with the process
    /// environment winning when both are set: `(discord_token, telegram_token,
    /// allowlist)`. Empty/blank tokens are treated as missing.
    ///
    /// This is what `gateway run` calls. Tokens the Full Setup wizard collects
    /// land in `<data_dir>/gateway.env` via the secrets store
    /// (`pantheon_secrets::gateway_token`); an exported
    /// `PANTHEON_TELEGRAM_BOT_TOKEN` / `PANTHEON_DISCORD_TOKEN` always
    /// overrides the stored one. [`read_channel_env`] stays the pure
    /// env-only reader for callers that must not touch the filesystem.
    /// Callers feed the tokens into [`ChannelPlan`] to decide what starts.
    pub fn read_channel_tokens(
        data_dir: &std::path::Path,
    ) -> (
        Option<String>,
        Option<String>,
        std::collections::HashSet<String>,
    ) {
        let token = |name: &str| {
            pantheon_secrets::gateway_token(data_dir, name)
                .map(|v| v.expose().to_string())
                .filter(|t| !t.trim().is_empty())
        };
        (
            token(pantheon_secrets::DISCORD_TOKEN_NAME),
            token(pantheon_secrets::TELEGRAM_TOKEN_NAME),
            read_allowlist(),
        )
    }

    /// Comma-separated platform ids:
    ///   PANTHEON_GATEWAY_ALLOW=6123456789,223344556677889900
    /// The allowlist stays env-only: it is an operator control, not a secret
    /// the wizard collects.
    fn read_allowlist() -> std::collections::HashSet<String> {
        std::env::var("PANTHEON_GATEWAY_ALLOW")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .map(|id| id.trim().to_string())
                    .filter(|id| !id.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Read the channel env without exiting: `(discord_token, telegram_token,
    /// allowlist)`. Callers feed this into [`ChannelPlan`] to decide what
    /// starts. Empty/blank tokens are treated as missing.
    pub fn read_channel_env() -> (
        Option<String>,
        Option<String>,
        std::collections::HashSet<String>,
    ) {
        let present = |name: &str| std::env::var(name).ok().filter(|t| !t.trim().is_empty());
        (
            present("PANTHEON_DISCORD_TOKEN"),
            present("PANTHEON_TELEGRAM_BOT_TOKEN"),
            read_allowlist(),
        )
    }

    /// One-line warning printed when `gateway run` starts with no chat surface.
    /// Printed, not fatal: the scheduler loop is the point of a scheduler-only
    /// install.
    pub fn channels_disabled_note() -> &'static str {
        "channel surfaces disabled: set PANTHEON_TELEGRAM_BOT_TOKEN or \
     PANTHEON_DISCORD_TOKEN and PANTHEON_GATEWAY_ALLOW to enable chat \
     control; the scheduler loop is running"
    }

    // ── stop / restart ──────────────────────────────────────────────────────
    // The TUI's `gateway stop|restart` and `pantheon init` share this, so the
    // two can never disagree about how the service is managed. `status` never
    // installs; `stop` never uninstalls.

    /// What [`stop_with`] did.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum StopOutcome {
        /// The service was stopped (the install remains).
        Stopped { mechanism: Mechanism },
        /// Nothing installed.
        NotInstalled,
        /// The mechanism has nothing to stop (a cron `@reboot` entry only
        /// fires at boot): an honest no-op, not a silent success.
        Noop { mechanism: Mechanism, note: String },
        /// A manager was found but the stop failed.
        Failed { mechanism: Mechanism, error: String },
    }

    /// Stop the background service without removing its install. Idempotent,
    /// never prompts, fails open.
    pub fn stop() -> StopOutcome {
        stop_with(&InstallEnv::real())
    }

    /// [`stop`] with an injectable environment (tests).
    pub fn stop_with(env: &InstallEnv) -> StopOutcome {
        let installed = match status_with(env).installed {
            Some(m) => m,
            None => return StopOutcome::NotInstalled,
        };
        let res: Result<(), String> = match installed {
            Mechanism::Systemd => run_cmd(env, "systemctl", &["--user", "stop", UNIT_NAME]),
            Mechanism::Launchd => match current_uid(env) {
                Ok(uid) => run_cmd(
                    env,
                    "launchctl",
                    &["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")],
                ),
                Err(e) => Err(e),
            },
            Mechanism::Cron => {
                return StopOutcome::Noop {
                    mechanism: installed,
                    note: "the cron @reboot entry starts the gateway at boot; there is \
                       no running service to stop - kill any foreground \
                       `pantheon gateway run` yourself"
                        .to_string(),
                }
            }
            Mechanism::TaskScheduler => run_cmd(env, "schtasks", &["/End", "/TN", TASK_NAME]),
            Mechanism::None => return StopOutcome::NotInstalled,
        };
        match res {
            Ok(()) => StopOutcome::Stopped {
                mechanism: installed,
            },
            Err(e) => {
                // Idempotent stop: failing to stop something that is not
                // running is success (launchd bootout of an unloaded label,
                // systemctl stop racing a dead unit).
                if !status_with(env).running {
                    StopOutcome::Stopped {
                        mechanism: installed,
                    }
                } else {
                    StopOutcome::Failed {
                        mechanism: installed,
                        error: e,
                    }
                }
            }
        }
    }

    /// What [`restart_with`] did.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum RestartOutcome {
        /// The service was restarted.
        Restarted { mechanism: Mechanism },
        /// The mechanism has nothing to restart (a cron `@reboot` entry only
        /// fires at boot): an honest no-op, not a silent success.
        Noop { mechanism: Mechanism, note: String },
        /// Nothing installed.
        NotInstalled,
        /// A manager was found but the restart failed.
        Failed { mechanism: Mechanism, error: String },
    }

    /// Restart the background service. Idempotent, never prompts.
    pub fn restart() -> RestartOutcome {
        restart_with(&InstallEnv::real())
    }

    /// [`restart`] with an injectable environment (tests).
    pub fn restart_with(env: &InstallEnv) -> RestartOutcome {
        let installed = match status_with(env).installed {
            Some(m) => m,
            None => return RestartOutcome::NotInstalled,
        };
        let res: Result<(), String> = match installed {
            Mechanism::Systemd => run_cmd(env, "systemctl", &["--user", "restart", UNIT_NAME]),
            Mechanism::Launchd => match current_uid(env) {
                Ok(uid) => {
                    let domain = format!("gui/{uid}");
                    let path = env
                        .launchd_agents_dir
                        .join(format!("{LAUNCHD_LABEL}.plist"));
                    // bootout first, ignoring "not loaded": restart must work
                    // whether or not the old generation is still up.
                    let _ = run_cmd(
                        env,
                        "launchctl",
                        &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
                    );
                    run_cmd(
                        env,
                        "launchctl",
                        &["bootstrap", &domain, &path.to_string_lossy()],
                    )
                }
                Err(e) => Err(e),
            },
            Mechanism::Cron => {
                return RestartOutcome::Noop {
                    mechanism: installed,
                    note: "the cron @reboot entry only fires at boot, so restart is a no-op; \
                       run `pantheon gateway run` in the foreground to pick up changes now"
                        .to_string(),
                }
            }
            Mechanism::TaskScheduler => run_cmd(env, "schtasks", &["/End", "/TN", TASK_NAME])
                .and_then(|()| run_cmd(env, "schtasks", &["/Run", "/TN", TASK_NAME])),
            Mechanism::None => return RestartOutcome::NotInstalled,
        };
        match res {
            Ok(()) => RestartOutcome::Restarted {
                mechanism: installed,
            },
            Err(e) => RestartOutcome::Failed {
                mechanism: installed,
                error: e,
            },
        }
    }
}
pub mod watchdog {
    //! Hung-gateway watchdog.
    //!
    //! A gateway poller thread that stops ticking is hung, and its channel
    //! stops delivering in both directions, and no in-process check can
    //! unstick a thread blocked inside a transport call (Rust cannot kill
    //! a thread). The supervisor therefore abandons the hung thread and
    //! spawns a replacement poller - exactly ONCE per channel. A second
    //! hang is not restarted. The supervisor reports the failure (loud log
    //! + a persistent `watchdog-<channel>.json` report under the gateway
    //!   state dir, which the operator - and the agent, when asked - can
    //!   read) and stops supervising that channel. No infinite restart loop,
    //!   because a channel that hangs, restarts, and hangs again is broken, and
    //!   restarting it forever would flap its cursors and duplicate
    //!   deliveries while hiding the breakage.
    //!
    //! This module is the decision logic only (clock-injectable, unit
    //! tested). The heartbeat itself is a shared millisecond timestamp
    //! the daemon loop touches each iteration
    //! ([`ChannelDaemon::with_heartbeat`](crate::runtime::daemon::ChannelDaemon::with_heartbeat));
    //! the supervisor thread in the gateway runner owns one
    //! [`GatewayWatchdog`] per channel and acts on its verdicts.

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// How long a poller may go without a heartbeat before it counts as
    /// hung. Must exceed one Telegram long-poll cycle (25 s) plus
    /// scheduling slack, or healthy long polls would false-positive.
    pub const HANG_BUDGET: Duration = Duration::from_secs(90);

    /// What the supervisor should do about a channel right now.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WatchdogAction {
        /// Heartbeat fresh (or not yet due): nothing to do.
        Ok,
        /// First hang: abandon the hung poller and spawn a replacement once.
        Restart,
        /// Second hang (or later): do NOT restart again. Report the
        /// failure and stop supervising this channel.
        Report,
    }

    /// Shared heartbeat timestamp (epoch millis) a daemon loop touches
    /// each iteration. `0` = never ticked.
    #[derive(Debug, Default)]
    pub struct GatewayHeartbeat {
        last_ms: AtomicU64,
    }

    impl GatewayHeartbeat {
        pub fn new() -> Self {
            Self::default()
        }

        /// Record a tick at wall-clock now.
        pub fn touch(&self) {
            self.last_ms.store(now_ms(), Ordering::Release);
        }

        /// Last tick's epoch millis, or `0` when the loop never ticked.
        pub fn last_ms(&self) -> u64 {
            self.last_ms.load(Ordering::Acquire)
        }
    }

    /// Wall-clock epoch millis (test seam: tests drive `check_at` /
    /// `heartbeat_at` with explicit timestamps instead).
    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// One channel's restart budget: one restart, then report-only.
    #[derive(Debug)]
    pub struct GatewayWatchdog {
        budget_ms: u64,
        last_heartbeat_ms: u64,
        started_ms: u64,
        restarted: bool,
        reported: bool,
    }

    impl GatewayWatchdog {
        pub fn new(budget: Duration) -> Self {
            Self::new_at(budget, now_ms())
        }

        /// Deterministic constructor for tests: `now_ms` is the supervisor
        /// start time, which counts as the initial heartbeat (a freshly
        /// spawned poller has not ticked yet, but it is not hung).
        pub fn new_at(budget: Duration, now_ms: u64) -> Self {
            Self {
                budget_ms: budget.as_millis() as u64,
                last_heartbeat_ms: now_ms,
                started_ms: now_ms,
                restarted: false,
                reported: false,
            }
        }

        /// Record a heartbeat at wall-clock now.
        pub fn heartbeat(&mut self) {
            self.heartbeat_at(now_ms());
        }

        /// Record a heartbeat at an explicit timestamp.
        pub fn heartbeat_at(&mut self, at_ms: u64) {
            if at_ms > self.last_heartbeat_ms {
                self.last_heartbeat_ms = at_ms;
            }
        }

        /// Evaluate at wall-clock now.
        pub fn check(&mut self) -> WatchdogAction {
            self.check_at(now_ms())
        }

        /// Evaluate at an explicit timestamp.
        ///
        /// - Fresh heartbeat (or no heartbeat yet but still inside the
        ///   budget measured from start) → `Ok`.
        /// - First breach → `Restart` (consumes the one restart).
        /// - Any later breach → `Report`, sticky: the supervisor must not
        ///   restart again, and repeated checks keep reporting rather
        ///   than silently recovering.
        ///
        /// A heartbeat after a restart re-arms the budget. A recovered
        /// channel is healthy again, but the restart budget is NOT
        /// refunded - a channel that hangs, recovers, and hangs again
        /// still reports instead of restarting a second time.
        pub fn check_at(&mut self, at_ms: u64) -> WatchdogAction {
            if self.reported {
                return WatchdogAction::Report;
            }
            let silent = at_ms.saturating_sub(self.last_heartbeat_ms);
            if silent <= self.budget_ms {
                return WatchdogAction::Ok;
            }
            if !self.restarted {
                self.restarted = true;
                return WatchdogAction::Restart;
            }
            self.reported = true;
            WatchdogAction::Report
        }

        /// True once the one restart has been consumed (even if the
        /// channel recovered afterwards).
        pub fn restarted(&self) -> bool {
            self.restarted
        }

        /// True once the watchdog has escalated to report-only.
        pub fn reported(&self) -> bool {
            self.reported
        }

        /// Supervisor start time (epoch millis), for report files.
        pub fn started_ms(&self) -> u64 {
            self.started_ms
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn budget() -> Duration {
            Duration::from_secs(90)
        }

        #[test]
        fn fresh_heartbeat_is_ok_and_silence_within_budget_is_ok() {
            let mut w = GatewayWatchdog::new_at(budget(), 1_000);
            assert_eq!(w.check_at(1_000 + 89_000), WatchdogAction::Ok);
            w.heartbeat_at(1_000 + 89_000);
            assert_eq!(w.check_at(1_000 + 89_000 + 90_000), WatchdogAction::Ok);
        }

        #[test]
        fn first_breach_restarts_once_then_second_breach_reports() {
            let mut w = GatewayWatchdog::new_at(budget(), 1_000);
            assert_eq!(w.check_at(1_000 + 90_001), WatchdogAction::Restart);
            assert!(w.restarted());
            // The replacement poller heartbeats: healthy again, but the
            // restart budget is spent.
            w.heartbeat_at(1_000 + 91_000);
            assert_eq!(w.check_at(1_000 + 91_000 + 90_000), WatchdogAction::Ok);
            assert_eq!(w.check_at(1_000 + 91_000 + 90_001), WatchdogAction::Report);
            assert!(w.reported());
            // Sticky: no second restart, ever.
            assert_eq!(w.check_at(1_000 + 500_000), WatchdogAction::Report);
        }

        #[test]
        fn no_heartbeat_after_restart_reports_without_another_restart() {
            let mut w = GatewayWatchdog::new_at(budget(), 1_000);
            assert_eq!(w.check_at(1_000 + 90_001), WatchdogAction::Restart);
            // The replacement never ticks: next check reports, not restarts.
            assert_eq!(w.check_at(1_000 + 180_002), WatchdogAction::Report);
        }
    }
}
pub mod dedup {
    //! Inbound dedup (§16).
    //!
    //! Gateways redeliver after a reconnect or a dropped ack, and the agent must
    //! nevertheless see each message exactly once. Message ids are only unique
    //! within a surface, so keys are namespaced by gateway and user.
    //!
    //! The window is bounded: a redelivery older than `capacity` messages is
    //! indistinguishable from a new message. Keep the capacity comfortably above
    //! the largest reconnect backlog a surface can replay.

    use crate::Identity;
    use std::collections::{HashSet, VecDeque};

    /// Dedup key for one inbound message.
    pub fn dedup_key(from: &Identity, message_id: &str) -> String {
        format!("{}:{}:{}", from.gateway, from.user, message_id)
    }

    /// Bounded set of recently seen inbound messages, oldest evicted first.
    #[derive(Debug)]
    pub struct DedupWindow {
        capacity: usize,
        order: VecDeque<String>,
        seen: HashSet<String>,
    }

    impl DedupWindow {
        /// A window that remembers the last `capacity` messages. Zero capacity
        /// dedups nothing, which is only correct for a non-redelivering surface.
        pub fn new(capacity: usize) -> Self {
            Self {
                capacity,
                order: VecDeque::new(),
                seen: HashSet::new(),
            }
        }

        /// Record a message. `true` means it is new and should be processed;
        /// `false` means it is a redelivery that must be dropped.
        pub fn observe(&mut self, from: &Identity, message_id: &str) -> bool {
            let key = dedup_key(from, message_id);
            if self.seen.contains(&key) {
                return false;
            }
            if self.capacity == 0 {
                return true;
            }
            self.seen.insert(key.clone());
            self.order.push_back(key);
            while self.order.len() > self.capacity {
                if let Some(evicted) = self.order.pop_front() {
                    self.seen.remove(&evicted);
                }
            }
            true
        }

        pub fn len(&self) -> usize {
            self.seen.len()
        }

        pub fn is_empty(&self) -> bool {
            self.seen.is_empty()
        }
    }
}
