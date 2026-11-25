//! Steering: a message for Pantheon while its run is open. Steering never stops
//! the run. Each `serve acp` conversation takes a second `session/prompt` as a
//! live steer; when no turn can take it (startup, between turns, refused), the
//! run holds it and sends it as the next turn of the same session the moment the
//! current turn ends. Each steer reports `steer` events: `pending` with its mode
//! on acceptance, `delivered` once the agent takes it, and `failed` if the run
//! ends first, so the app can queue it again.

use super::goals::RpcReply;
use super::*;

const MAX_STEER_BYTES: usize = 128 * 1024;
const MAX_STEERS: usize = 16;

impl Protocol {
    pub(super) fn steer(&mut self, id: String, text: String, reply: RpcReply) -> Step {
        let step = Step::default();
        let text = text.trim().to_owned();
        if text.is_empty() || text.len() > MAX_STEER_BYTES {
            let _ = reply.send(Err("Write a steering message under 128 KiB.".into()));
            return step;
        }
        if self.acp_steers.live.len() + self.acp_steers.held.len() >= MAX_STEERS {
            let _ = reply.send(Err(
                "Too many steering messages are waiting; the message stays queued.".into(),
            ));
            return step;
        }
        self.acp_steer(id, text, reply)
    }

    /// The run ended. Steers it never delivered are reported failed so the app can
    /// queue them again; after a clean finish, an accepted live steer was taken.
    pub(super) fn steers_ended(&mut self, clean: bool) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        for (id, text) in std::mem::take(&mut self.acp_steers.held) {
            events.push(self.steer_event(&id, &text, "failed", "next-turn"));
        }
        for (_request, id, text) in std::mem::take(&mut self.acp_steers.live) {
            events.push(self.steer_event(
                &id,
                &text,
                if clean { "delivered" } else { "failed" },
                "live",
            ));
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::config;
    use super::*;

    fn steer(
        protocol: &mut Protocol,
        text: &str,
    ) -> (Step, tokio::sync::oneshot::Receiver<Result<Value, String>>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = format!("steer-{text}");
        (protocol.steer(id, text.into(), tx), rx)
    }

    fn steer_events(step: &Step) -> Vec<(String, String)> {
        step.events
            .iter()
            .filter(|event| event.kind == "steer")
            .map(|event| {
                let data = event.data.as_ref().unwrap();
                (
                    data["status"].as_str().unwrap().to_owned(),
                    data["mode"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn empty_and_oversized_steers_are_refused_before_touching_the_session() {
        let mut p = Protocol::new(config("pantheon"));
        let (step, mut reply) = steer(&mut p, "   ");
        assert!(step.write.is_empty());
        assert!(reply.try_recv().unwrap().is_err());
        let (step, mut reply) = steer(&mut p, &"x".repeat(MAX_STEER_BYTES + 1));
        assert!(step.write.is_empty());
        assert!(reply.try_recv().unwrap().is_err());
    }

    #[test]
    fn a_failed_or_stopped_run_reports_undelivered_steers() {
        let mut p = Protocol::new(config("pantheon"));
        p.phase = 1;
        p.remote_id = "session".into();
        let (step, mut reply) = steer(&mut p, "also check the docs");
        assert_eq!(step.write.len(), 1);
        assert_eq!(step.write[0]["method"], "session/prompt");
        assert_eq!(reply.try_recv().unwrap().unwrap()["mode"], "live");
        assert_eq!(
            steer_events(&step),
            [
                ("pending".to_string(), "live".to_string()),
                ("delivered".to_string(), "live".to_string())
            ]
        );
        let events = p.steers_ended(false);
        assert_eq!(events[0].data.as_ref().unwrap()["status"], "failed");
    }
}
