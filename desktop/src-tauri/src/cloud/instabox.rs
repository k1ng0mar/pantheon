//! InstaBox (instabox.sh) machines for cloud agents (docs/cloud-agents.md), over the v1 REST
//! API with an `X-API-Key`. Lifecycle calls return operations, polled at `/operations/{id}`;
//! every mutation carries a fresh `Idempotency-Key`. Machines suspend with their memory, so
//! processes freeze rather than die.
//!
//! `exec` runs `bash -lc`, waits at most 600 s and keeps only the *first* 1 MiB of output, so
//! each command runs from a script on the machine with its output in a file: small output
//! comes back in the same call, larger output is read back in 128 KiB pieces keeping the
//! tail. Longer commands run detached and are polled for an exit-code file.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::{http, sandbox::{download_command, Info, Sandbox}, Machine};
use crate::Result;

const SERVICE: &str = "InstaBox";
const DEFAULT_API: &str = "https://api.instabox.sh/v1";
const KEY: &str = "instabox_key";
/// The template the free plan allows.
const TEMPLATE: &str = "agent-browser";
/// Metadata on every machine Pantheon creates; `list` shows only these.
const LABEL: (&str, &str) = ("created-by", "pantheon");
/// InstaBox's longest waiting `exec`, in seconds.
const WAIT_MAX: u64 = 600;
/// Output returned in the command's own reply; InstaBox keeps the first 1 MiB of stdout.
const INLINE: u64 = 1_000_000;
/// Output kept for a caller (the `exec` contract asks for at least 4 MiB).
const KEEP: u64 = 4 * 1024 * 1024;
/// Bytes per read-back call.
const PIECE: usize = 128 * 1024;
/// The longest activity lease InstaBox grants, in seconds.
const LEASE_MAX: u64 = 3600;
const OPERATION_MAX: Duration = Duration::from_secs(600);
const POLL: Duration = if cfg!(test) { Duration::from_millis(10) } else { Duration::from_secs(3) };

pub struct InstaBox {
    api: String,
    key: String,
}

fn machine(vm: &Value) -> Machine {
    let id = vm["id"].as_str().unwrap_or_default();
    let state = match vm["state"].as_str().unwrap_or("unknown") {
        "running" if vm["runtime"]["ready"].as_bool() == Some(false) => "starting",
        "running" => "running",
        "creating" | "starting" | "resuming" => "starting",
        "stopping" | "suspending" | "deleting" => "stopping",
        "suspended" | "stopped" => "stopped",
        other => other,
    };
    Machine {
        id: format!("instabox:{id}"),
        provider: "instabox".into(),
        name: vm["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(id).to_string(),
        state: state.into(),
        os: "linux".into(),
        desktop: false,
        detail: format!("InstaBox · {}", vm["templateId"].as_str().unwrap_or(TEMPLATE)),
    }
}

/// InstaBox machine IDs go into URL paths, so only plain IDs are accepted.
fn valid_id(id: &str) -> Result<&str> {
    let plain = (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    plain.then_some(id).ok_or_else(|| "Unknown InstaBox machine.".into())
}

/// Prints `<exit code> <output bytes>`, then the output itself (and cleans up) when it fits.
fn report(file: &str) -> String {
    format!("s=$(wc -c <{file}.out | tr -d ' '); echo \"$(cat {file}.code) $s\"; if [ \"$s\" -le {INLINE} ]; then cat {file}.out; rm -f {file}.*; fi")
}

impl InstaBox {
    pub fn new(api: Option<String>, key: String) -> Self {
        InstaBox { api: api.unwrap_or_else(|| DEFAULT_API.into()).trim_end_matches('/').to_string(), key }
    }
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        let mut request = http::client(timeout)?.request(method.clone(), format!("{}{path}", self.api)).header("X-API-Key", &self.key);
        if method != reqwest::Method::GET {
            request = request.header("Idempotency-Key", uuid::Uuid::new_v4().to_string());
        }
        http::send(SERVICE, match body { Some(body) => request.json(&body), None => request }).await
    }
    /// Wait for an operation to succeed and return its final state.
    async fn settle(&self, operation: Value) -> Result<Value> {
        let id = operation["id"].as_str().ok_or("InstaBox did not return an operation.")?.to_string();
        let (deadline, mut current) = (Instant::now() + OPERATION_MAX, operation);
        loop {
            match current["state"].as_str() {
                Some("succeeded") => return Ok(current),
                Some("failed" | "canceled" | "cancelled") => {
                    let what = current["type"].as_str().unwrap_or("operation");
                    return Err(format!("InstaBox could not finish the {what}: {}", current["error"]["message"].as_str().unwrap_or("no reason given")));
                }
                _ if Instant::now() >= deadline => return Err("InstaBox is still working on it; try again in a minute.".into()),
                _ => {}
            }
            tokio::time::sleep(POLL).await;
            current = self.call(reqwest::Method::GET, &format!("/operations/{id}"), None, Duration::from_secs(30)).await?;
        }
    }
    async fn get(&self, id: &str) -> Result<Value> {
        let id = valid_id(id)?;
        self.call(reqwest::Method::GET, &format!("/machines/{id}"), None, Duration::from_secs(30)).await
    }
    async fn list(&self) -> Result<Vec<Machine>> {
        let (mut machines, mut cursor) = (Vec::new(), None::<String>);
        loop {
            let mut query = reqwest::Url::parse("https://x/").map_err(|e| e.to_string())?;
            query.query_pairs_mut().append_pair("metadata", &format!("{}:{}", LABEL.0, LABEL.1)).extend_pairs(cursor.iter().map(|c| ("cursor", c)));
            let page = self.call(reqwest::Method::GET, &format!("/machines?{}", query.query().unwrap_or_default()), None, Duration::from_secs(30)).await?;
            // Filter again: never show or touch a machine without Pantheon's label.
            machines.extend(page["machines"].as_array().into_iter().flatten().filter(|m| m["metadata"][LABEL.0] == LABEL.1).map(machine));
            cursor = page["nextCursor"].as_str().filter(|c| !c.is_empty()).map(str::to_string);
            if cursor.is_none() {
                return Ok(machines);
            }
        }
    }
    async fn create(&self, name: &str) -> Result<Machine> {
        let body = json!({"name": name, "templateId": TEMPLATE, "metadata": {LABEL.0: LABEL.1}});
        let operation = self.call(reqwest::Method::POST, "/machines", Some(body), Duration::from_secs(60)).await?;
        let id = operation["machineId"].as_str().map(str::to_string);
        let done = self.settle(operation).await?;
        let id = done["machineId"].as_str().map(str::to_string).or(id).ok_or("InstaBox did not return a machine ID.")?;
        Ok(machine(&self.get(&id).await?))
    }
    /// The machine, only when Pantheon created it.
    async fn owned(&self, id: &str) -> Result<Value> {
        let vm = self.get(id).await?;
        if vm["metadata"][LABEL.0] != LABEL.1 {
            return Err("Pantheon only controls InstaBox machines it created.".into());
        }
        Ok(vm)
    }
    async fn power(&self, id: &str, action: &str) -> Result<()> {
        let timeout = Duration::from_secs(60);
        if !matches!(action, "start" | "stop" | "delete") {
            return Err(format!("InstaBox machines can start, stop or be deleted, not {action}."));
        }
        let state = self.owned(id).await?["state"].as_str().unwrap_or_default().to_string();
        let operation = match (action, state.as_str()) {
            ("start", "suspended") => self.call(reqwest::Method::POST, &format!("/machines/{id}/resume"), Some(json!({})), timeout).await?,
            ("start", "stopped") => self.call(reqwest::Method::POST, &format!("/machines/{id}/start"), Some(json!({})), timeout).await?,
            ("stop", "running") => self.call(reqwest::Method::POST, &format!("/machines/{id}/suspend"), Some(json!({})), timeout).await?,
            ("delete", _) => self.call(reqwest::Method::DELETE, &format!("/machines/{id}"), None, timeout).await?,
            _ => return Ok(()),
        };
        if operation.is_null() { Ok(()) } else { self.settle(operation).await.map(|_| ()) }
    }
    async fn keep_alive(&self, id: &str, minutes: u32) -> Result<()> {
        self.owned(id).await?;
        let seconds = u64::from(minutes.max(1)) * 60;
        let patched = self.call(reqwest::Method::PATCH, &format!("/machines/{id}"), Some(json!({"autoSuspendAfterSeconds": seconds})), Duration::from_secs(30)).await;
        if patched.is_ok() {
            return Ok(());
        }
        // Plans with a fixed idle pause refuse the PATCH; an activity lease holds it off instead.
        let lease = json!({"machineId": id, "ttlSeconds": seconds.min(LEASE_MAX)});
        self.call(reqwest::Method::PUT, &format!("/activity-leases/pantheon-{id}"), Some(lease), Duration::from_secs(30)).await.map(|_| ())
    }
    /// One `exec` call; the reply's stdout, or None when InstaBox stopped it at `seconds`.
    async fn run(&self, id: &str, cmd: &str, stdin: Option<&str>, seconds: u64) -> Result<Option<String>> {
        let mut body = json!({"cmd": cmd, "timeoutSeconds": seconds.clamp(1, WAIT_MAX)});
        if let Some(stdin) = stdin {
            body["stdin"] = stdin.into();
        }
        let result = self.call(reqwest::Method::POST, &format!("/machines/{id}/exec"), Some(body), Duration::from_secs(seconds + 70)).await?;
        Ok((result["timedOut"].as_bool() != Some(true)).then(|| result["stdout"].as_str().unwrap_or_default().to_string()))
    }
    async fn exec(&self, id: &str, command: &str, timeout: Duration) -> Result<(Option<i32>, String)> {
        self.owned(id).await?;
        let file = format!("/tmp/pantheon-exec-{}", uuid::Uuid::new_v4().simple());
        let seconds = timeout.as_secs().max(1);
        // The command arrives on stdin, so it needs no quoting and stays within the 64 KiB `cmd`.
        let save = format!("cat >{file}.sh; ");
        let reply = if seconds <= WAIT_MAX {
            let script = format!("{save}bash {file}.sh >{file}.out 2>&1 </dev/null; echo $? >{file}.code; {}", report(&file));
            match self.run(id, &script, Some(command), seconds).await? {
                Some(reply) => reply,
                None => {
                    let _ = self.run(id, &format!("rm -f {file}.*"), None, 30).await;
                    return Ok((None, format!("Timed out after {seconds} s.")));
                }
            }
        } else {
            let detached = format!("bash {file}.sh >{file}.out 2>&1 </dev/null; echo $? >{file}.tmp; mv {file}.tmp {file}.code");
            let script = format!("{save}setsid nohup sh -c '{detached}' >/dev/null 2>&1 </dev/null & echo $! >{file}.pid");
            self.run(id, &script, Some(command), 60).await?.ok_or("InstaBox did not start the command.")?;
            let (deadline, poll) = (Instant::now() + timeout, format!("[ -f {file}.code ] && {{ {}; }}", report(&file)));
            loop {
                if let Some(reply) = self.run(id, &poll, None, 60).await?.filter(|r| !r.is_empty()) {
                    break reply;
                }
                if Instant::now() >= deadline {
                    let _ = self.run(id, &format!("kill -TERM -- -$(cat {file}.pid) 2>/dev/null; rm -f {file}.*"), None, 30).await;
                    return Ok((None, format!("Timed out after {seconds} s.")));
                }
                tokio::time::sleep(POLL).await;
            }
        };
        self.collect(id, &file, &reply).await
    }
    /// The exit code and output from a `report` reply, reading large output back in pieces.
    async fn collect(&self, id: &str, file: &str, reply: &str) -> Result<(Option<i32>, String)> {
        let (header, inline) = reply.split_once('\n').unwrap_or((reply, ""));
        let mut fields = header.split_whitespace();
        let code = fields.next().and_then(|c| c.parse().ok());
        let size: u64 = fields.next().and_then(|s| s.parse().ok()).ok_or("InstaBox sent an unreadable command result.")?;
        if size <= INLINE {
            return Ok((code, inline.to_string()));
        }
        let (output, mut offset, mut bytes) = (format!("{file}.out"), size.saturating_sub(KEEP), Vec::new());
        while offset < size {
            let piece = self.run(id, &download_command(&output, offset, PIECE), None, 120).await?.unwrap_or_default();
            let piece = STANDARD.decode(piece.trim()).map_err(|_| "The cloud machine sent output Pantheon could not read.")?;
            if piece.is_empty() {
                break;
            }
            offset += piece.len() as u64;
            bytes.extend(piece);
        }
        let _ = self.run(id, &format!("rm -f {file}.*"), None, 30).await;
        Ok((code, String::from_utf8_lossy(&bytes).into_owned()))
    }
}

impl Sandbox for InstaBox {
    /// Suspended machines keep disk and memory. The free plan pauses after a fixed 30 idle
    /// minutes, so promise no more than that.
    fn info(&self) -> Info {
        Info { persistent: true, max_unattended_minutes: 30 }
    }
    fn list(&self) -> Result<Vec<Machine>> {
        http::block(InstaBox::list(self))
    }
    fn create(&self, name: &str) -> Result<Machine> {
        http::block(InstaBox::create(self, name))
    }
    fn power(&self, native: &str, action: &str) -> Result<()> {
        http::block(InstaBox::power(self, native, action))
    }
    fn exec(&self, native: &str, command: &str, timeout: Duration) -> Result<(Option<i32>, String)> {
        http::block(InstaBox::exec(self, native, command, timeout))
    }
    fn keep_alive(&self, native: &str, minutes: u32) -> Result<()> {
        http::block(InstaBox::keep_alive(self, native, minutes))
    }
}

/// Check the key with a real request, then save it.
pub fn connect(key: &str, _options: &Value) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        return Err("Enter an InstaBox API key (ibk_…).".into());
    }
    let count = http::block(InstaBox::new(None, key.to_string()).list())?.len();
    super::set_secret(KEY, key)?;
    let mut settings = super::settings();
    settings.providers.insert("instabox".into(), [("detail".to_string(), format!("InstaBox · {count} Pantheon machines"))].into());
    super::save_settings(&settings)
}
pub fn disconnect() -> Result<()> {
    super::remove_secret(KEY);
    let mut settings = super::settings();
    settings.providers.remove("instabox");
    super::save_settings(&settings)
}
pub fn sandbox() -> Result<Box<dyn Sandbox>> {
    let key = super::secret(KEY).ok_or("Connect InstaBox in Settings → Cloud first.")?;
    Ok(Box::new(InstaBox::new(None, key)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{http::mock, sandbox::checked};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    /// 1.5 MB of output whose only error is on the last line.
    fn big() -> Vec<u8> {
        let mut out = b"ok\n".repeat(500_000 - 4);
        out.extend_from_slice(b"error: boom\n");
        out
    }

    fn exec_reply(stdout: &str) -> (u16, Vec<u8>) {
        (200, json!({"stdout": stdout, "stderr": "", "exitCode": 0, "timedOut": false, "truncated": false}).to_string().into_bytes())
    }

    fn instabox() -> (InstaBox, mock::Seen) {
        let (polls, operations) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (base, seen) = mock::serve(move |request, _| -> (u16, Vec<u8>) {
            let body: Value = serde_json::from_str(&request.body).unwrap_or_default();
            let (cmd, stdin) = (body["cmd"].as_str().unwrap_or_default(), body["stdin"].as_str().unwrap_or_default());
            let text = |s: &str| (200, s.as_bytes().to_vec());
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/v1/machines?metadata=created-by%3Apantheon") => text(r#"{"machines":[{"id":"m_1","name":"pc-a","state":"running","runtime":{"ready":true},"metadata":{"created-by":"pantheon"}},{"id":"m_9","name":"other","state":"running"}],"nextCursor":"c/2"}"#),
                ("GET", "/v1/machines?metadata=created-by%3Apantheon&cursor=c%2F2") => text(r#"{"machines":[{"id":"m_2","name":"","state":"suspended","metadata":{"created-by":"pantheon"}}],"nextCursor":null}"#),
                ("POST", "/v1/machines") => (202, br#"{"id":"op_1","machineId":"m_3","type":"create","state":"pending"}"#.to_vec()),
                ("GET", "/v1/operations/op_1") if operations.fetch_add(1, Ordering::SeqCst) == 0 => text(r#"{"id":"op_1","machineId":"m_3","state":"running"}"#),
                ("GET", "/v1/operations/op_1") => text(r#"{"id":"op_1","machineId":"m_3","state":"succeeded"}"#),
                ("GET", "/v1/machines/m_3") => text(r#"{"id":"m_3","name":"pc-new","state":"running","runtime":{"ready":false}}"#),
                ("GET", "/v1/machines/m_2") => text(r#"{"id":"m_2","state":"suspended","metadata":{"created-by":"pantheon"}}"#),
                ("GET", "/v1/machines/m_1") => text(r#"{"id":"m_1","state":"running","metadata":{"created-by":"pantheon"}}"#),
                ("GET", "/v1/machines/m_9") => text(r#"{"id":"m_9","state":"running","metadata":{}}"#),
                ("POST", "/v1/machines/m_2/resume") => (202, br#"{"id":"op_2","state":"succeeded"}"#.to_vec()),
                ("DELETE", "/v1/machines/m_2") => (202, br#"{"id":"op_3","type":"delete","state":"failed","error":{"message":"busy"}}"#.to_vec()),
                ("PATCH", "/v1/machines/m_1") => (403, br#"{"error":{"code":"plan","message":"not on this plan"}}"#.to_vec()),
                ("PUT", "/v1/activity-leases/pantheon-m_1") => text("{}"),
                ("POST", "/v1/machines/m_1/exec") if cmd.contains("tail -c +") => {
                    let number = |after: &str| cmd.split(after).nth(1).and_then(|r| r.split(' ').next()).and_then(|n| n.parse::<usize>().ok()).unwrap();
                    let (start, max, all) = (number("tail -c +") - 1, number("head -c "), big());
                    exec_reply(&STANDARD.encode(&all[start.min(all.len())..(start + max).min(all.len())]))
                }
                ("POST", "/v1/machines/m_1/exec") if cmd.contains("setsid") => exec_reply(""),
                ("POST", "/v1/machines/m_1/exec") if cmd.starts_with("[ -f") => exec_reply(if polls.fetch_add(1, Ordering::SeqCst) == 0 { "" } else { "0 4\nlong" }),
                ("POST", "/v1/machines/m_1/exec") if stdin == "ls" => exec_reply("2 6\nouterr"),
                ("POST", "/v1/machines/m_1/exec") if stdin == "big" => exec_reply(&format!("1 {}\n", big().len())),
                ("POST", "/v1/machines/m_1/exec") if stdin == "sleep" => (200, json!({"stdout": "", "exitCode": -1, "timedOut": true}).to_string().into_bytes()),
                ("POST", "/v1/machines/m_1/exec") => exec_reply(""),
                _ => (404, br#"{"error":{"message":"not found"}}"#.to_vec()),
            }
        });
        (InstaBox::new(Some(format!("{base}/v1/")), "ibk_k".into()), seen)
    }

    #[tokio::test]
    async fn lists_creates_and_controls_machines_through_operations() {
        let (instabox, seen) = instabox();
        let listed = instabox.list().await.unwrap();
        assert_eq!(listed.iter().map(|m| (m.id.as_str(), m.name.as_str(), m.state.as_str())).collect::<Vec<_>>(), vec![("instabox:m_1", "pc-a", "running"), ("instabox:m_2", "m_2", "stopped")]);
        let created = instabox.create("pc-new").await.unwrap();
        assert_eq!((created.id.as_str(), created.state.as_str()), ("instabox:m_3", "starting"));
        instabox.power("m_2", "start").await.unwrap();
        assert!(instabox.power("m_2", "delete").await.unwrap_err().contains("busy"));
        assert!(instabox.power("m_2", "pause").await.is_err());
        instabox.keep_alive("m_1", 30).await.unwrap();
        let log = seen.lock().unwrap();
        assert_eq!(log.iter().filter(|r| r.path == "/v1/operations/op_1").count(), 2, "polled until it succeeded");
        assert!(log.iter().all(|r| r.headers.iter().any(|(n, v)| n == "x-api-key" && v == "ibk_k")));
        let keys: Vec<&str> = log.iter().filter(|r| r.method != "GET").map(|r| r.headers.iter().find(|(n, _)| n == "idempotency-key").map(|(_, v)| v.as_str()).expect("mutation without an Idempotency-Key")).collect();
        assert_eq!(keys.len(), 5);
        assert_eq!(keys.iter().collect::<std::collections::HashSet<_>>().len(), keys.len(), "each mutation gets its own key");
        assert!(log.iter().filter(|r| r.method == "GET").all(|r| !r.headers.iter().any(|(n, _)| n == "idempotency-key")));
        let create: Value = serde_json::from_str(&log.iter().find(|r| r.method == "POST" && r.path == "/v1/machines").unwrap().body).unwrap();
        assert_eq!(create, json!({"name": "pc-new", "templateId": "agent-browser", "metadata": {"created-by": "pantheon"}}));
        let lease: Value = serde_json::from_str(&log.iter().find(|r| r.method == "PUT").unwrap().body).unwrap();
        assert_eq!(lease, json!({"machineId": "m_1", "ttlSeconds": 1800}));
    }

    #[tokio::test]
    async fn runs_commands_from_stdin_and_reads_large_output_tail() {
        let (instabox, seen) = instabox();
        assert_eq!(instabox.exec("m_1", "ls", Duration::from_secs(30)).await.unwrap(), (Some(2), "outerr".into()));
        let (code, output) = instabox.exec("m_1", "big", Duration::from_secs(30)).await.unwrap();
        assert_eq!((code, output.len()), (Some(1), big().len()));
        assert!(output.ends_with("error: boom\n"));
        assert!(checked((code, output)).unwrap_err().contains("error: boom"));
        assert_eq!(instabox.exec("m_1", "sleep", Duration::from_secs(5)).await.unwrap().0, None);
        let log = seen.lock().unwrap();
        let bodies: Vec<Value> = log.iter().filter(|r| r.method == "POST").map(|r| serde_json::from_str(&r.body).unwrap()).collect();
        let pieces: Vec<&Value> = bodies.iter().filter(|b| b["cmd"].as_str().unwrap().contains("tail -c +")).collect();
        assert_eq!(pieces.len(), big().len().div_ceil(PIECE));
        assert!(pieces.iter().all(|b| b["cmd"].as_str().unwrap().contains(&format!("head -c {PIECE}"))));
        let first = &bodies[0];
        assert_eq!((first["stdin"].as_str(), first["timeoutSeconds"].as_u64()), (Some("ls"), Some(30)));
        assert!(bodies.iter().all(|b| b["timeoutSeconds"].as_u64().unwrap() <= WAIT_MAX && b["cmd"].as_str().unwrap().len() < 64 * 1024));
    }

    #[tokio::test]
    async fn polls_commands_longer_than_ten_minutes() {
        let (instabox, seen) = instabox();
        assert_eq!(instabox.exec("m_1", "build", Duration::from_secs(1800)).await.unwrap(), (Some(0), "long".into()));
        let log = seen.lock().unwrap();
        assert_eq!(log.iter().filter(|r| r.body.contains("[ -f")).count(), 2);
    }

    #[tokio::test]
    async fn refuses_unlabelled_machines_and_unsafe_ids() {
        let (instabox, seen) = instabox();
        for action in ["start", "stop", "delete"] {
            assert!(instabox.power("m_9", action).await.unwrap_err().contains("it created"), "{action}");
        }
        assert!(instabox.exec("m_9", "ls", Duration::from_secs(30)).await.unwrap_err().contains("it created"));
        assert!(instabox.keep_alive("m_9", 30).await.unwrap_err().contains("it created"));
        for id in ["m_1/exec", "m_1?x", "m_1#x", "..", "../operations/op_1", "", &"a".repeat(129)] {
            assert_eq!(instabox.exec(id, "ls", Duration::from_secs(30)).await.unwrap_err(), "Unknown InstaBox machine.", "{id}");
            assert!(instabox.power(id, "delete").await.is_err(), "{id}");
        }
        let log = seen.lock().unwrap();
        assert!(log.iter().all(|r| r.method == "GET" && r.path == "/v1/machines/m_9"), "nothing else was sent");
    }

    #[test]
    fn connect_refuses_empty_keys() {
        assert!(connect(" ", &json!({})).is_err());
    }
}
