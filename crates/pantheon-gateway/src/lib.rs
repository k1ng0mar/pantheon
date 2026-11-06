//! Gateways (spec section 16): Telegram/Discord/Custom normalized into
//! canonical messages. The agent never knows which surface sent it.
//!
//! The interactive path (AG-UI) lives here too:
//! `stream` maps ledger events to UI frames, `sse` encodes frames as
//! `text/event-stream` bytes, `channel` is the transport seam every
//! surface (discord/slack/web/...) consumes, and `genui` mints
//! task-id + signed-URL references (never embedded payloads).
use serde::{Deserialize, Serialize};

pub mod channels;
pub mod runtime;
pub mod server;
pub mod voice_group;

// Crate-root shims for modules moved into groups by the gateway merge.
// External paths (`pantheon_gateway::http::...`, `pantheon_gateway::voice::...`,
// `pantheon_gateway::live_voice::...`) keep working; same pattern as the exec
// and providers merges.

pub mod http {
    pub use crate::server::http::*;
}
pub mod live_voice {
    pub use crate::voice_group::live_voice::*;
}
pub mod voice {
    pub use crate::voice_group::voice::*;
}
pub mod daemon {
    pub use crate::runtime::daemon::*;
}
pub mod schedule_delivery {
    pub use crate::channels::schedule_delivery::*;
}
pub mod service {
    pub use crate::runtime::service::*;
}
pub mod channel {
    pub use crate::channels::channel::*;
}
pub mod channel_voice {
    pub use crate::voice_group::channel_voice::*;
}
pub mod dedup {
    pub use crate::runtime::dedup::*;
}
pub mod delivery {
    pub use crate::channels::delivery::*;
}
pub mod discord {
    pub use crate::channels::discord::*;
}
pub mod discord_gateway {
    pub use crate::channels::discord_gateway::*;
}
pub mod genui {
    pub use crate::server::genui::*;
}
pub mod mcp_boot {
    pub use crate::server::mcp_boot::*;
}
pub mod notify {
    pub use crate::server::notify::*;
}
pub mod realtime_voice {
    pub use crate::voice_group::realtime_voice::*;
}
pub mod rendezvous {
    pub use crate::server::rendezvous::*;
}
pub mod scheduler {
    pub use crate::runtime::scheduler::*;
}
pub mod sse {
    pub use crate::server::sse::*;
}
pub mod stream {
    pub use crate::server::stream::*;
}
pub mod telegram {
    pub use crate::channels::telegram::*;
}
pub mod watchdog {
    pub use crate::runtime::watchdog::*;
}

pub use channels::channel::{
    chunk_text, fanout, format_text, ApprovalButtons, Channel, ChannelEnvelope, ChannelError,
    ChannelEvent, MemoryChannel, ThreadRunMap,
};
pub use channels::delivery::{backoff_ms, plan_delivery, DeliveryOutcome, Outbox};
pub use channels::discord::{
    parse_event as parse_discord_event, DiscordChannel, DiscordRestTransport, DiscordTransport,
    DISCORD_CONTENT_LIMIT,
};
pub use channels::schedule_delivery::{
    build_summary, deliver_best_effort, deliver_summary, deliver_to_home_session,
    routes_via_home_session, ChannelSender, Deliver, RestChannelSender,
};
pub use channels::telegram::{
    parse_event as parse_telegram_event, TelegramChannel, TelegramRestTransport, TelegramTransport,
    TELEGRAM_MESSAGE_LIMIT,
};
pub use runtime::daemon::{
    poll_telegram_once, route_event, route_event_with_hook, route_outbound, ChannelDaemon,
    EventSink, PreDispatchHook, UpdateCursor, MAX_SEND_ATTEMPTS,
};
pub use runtime::dedup::{dedup_key, DedupWindow};
pub use runtime::watchdog::{GatewayHeartbeat, GatewayWatchdog, WatchdogAction, HANG_BUDGET};
pub use server::genui::{valid_task_id, GenUiRef, GenUiSigner, SignedUrl};
pub use server::notify::{
    command_for, desktop_notify, detect_notifier, escape_applescript, escape_powershell, Notifier,
};
pub use server::rendezvous::{
    is_running as gateway_singleton_running, try_acquire as gateway_try_acquire,
    AcquireOutcome as GatewayAcquireOutcome, GatewayLock,
};
pub use server::sse::{parse_last_event_id, SseEncoder};
pub use server::stream::{frame_for_event, frames_for_entries, UiFrame, UiFrameKind};
pub use voice_group::channel_voice::{
    ext_for_filename, ext_for_mime, VoiceOutcome, VoicePipes, VoiceSlot, MAX_VOICE_BYTES,
    STT_EMPTY, STT_FAILED, STT_NOT_CONFIGURED, STT_UNAVAILABLE, VOICE_TRANSCRIPT_PREFIX,
};
pub use voice_group::live_voice::{
    is_speech, pcm_rms, wav_to_pcm_16k, wav_wrap, LiveTurnDriver, LiveVoiceConfig, TurnOutcome,
    LIVE_TRANSCRIPT_PREFIX, LIVE_VOICE_PATH, SAMPLE_RATE,
};
pub use voice_group::voice::{
    content_type_for, SpeakBody, TranscribeBody, VoiceEdge, VoiceHttp, DEFAULT_REQUEST_TIMEOUT,
    MAX_AUDIO_BYTES, MAX_TEXT_CHARS, SPEAK_PATH, TRANSCRIBE_PATH,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub gateway: String,
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub bytes: usize,
}

/// A place a conversation happens: a channel, DM, or thread on a surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub gateway: String,
    /// Surface-native conversation id.
    pub id: String,
    /// Thread inside the conversation, when the surface has threads.
    pub thread: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    pub id: String,
    pub from: Identity,
    /// The canonical place this happened (§16): gateway + conversation +
    /// optional thread. Store the shape, not a bare id, so every surface
    /// reports the same keys for the same place.
    pub conversation: Conversation,
    pub text: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub to_conversation: String,
    pub text: String,
    /// Surface that owns this reply ("telegram"/"discord"). Each daemon
    /// drains its own queue, so the tag - not a shared vec - decides who
    /// sends what. Empty means unknown (older queue files).
    #[serde(default)]
    pub gateway: String,
    /// Delivery attempts so far. The daemon bumps it on every failed send
    /// and dead-letters the message at [`daemon::MAX_SEND_ATTEMPTS`]
    /// instead of requeueing forever.
    #[serde(default)]
    pub attempts: u32,
}

impl OutboundMessage {
    /// A fresh message for `to_conversation` on `gateway` ("telegram" /
    /// "discord"); zero attempts, ready for the daemon.
    pub fn new(to_conversation: &str, text: &str, gateway: &str) -> Self {
        Self {
            to_conversation: to_conversation.to_string(),
            text: text.to_string(),
            gateway: gateway.to_string(),
            attempts: 0,
        }
    }
}

pub use runtime::scheduler::{
    queue_summary, rel_time, ExecuteFn, FireOutcome, OutcomeSink, ScheduledJob, SchedulerLoop,
    TaskOutcome, TickReport,
};
pub use runtime::service::{
    channels_disabled_note, cron_line, detect as detect_service_mechanism,
    install as install_service, manual_cron_line, merge_crontab, read_channel_env,
    read_channel_tokens, render_launchd_plist, render_systemd_unit, render_task_xml,
    restart as restart_service, self_exe, status as service_status, stop as stop_service,
    ChannelPlan, InstallEnv, InstallOutcome, Mechanism as ServiceMechanism, RestartOutcome,
    ServiceStatus, StopOutcome, CRON_MARKER, LAUNCHD_LABEL, TASK_NAME, UNIT_NAME,
};
