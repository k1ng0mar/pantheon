//! A JSONL pipe to a child process, used by ACP transport and command discovery.
//! No unbounded line buffers and no detached readers: dropping the future kills
//! the process group and reaps it.
use crate::providers::ProcessGroup;
use serde_json::Value;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::Instant,
};

pub(crate) struct Pipe {
    child: Option<Child>,
    group: Option<ProcessGroup>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    deadline: Instant,
    bytes: usize,
    frames: usize,
    stderr_task: tokio::task::JoinHandle<String>,
    diagnostic: String,
}

impl Pipe {
    pub(crate) fn spawn(mut command: Command, cwd: &Path, timeout: Duration) -> Result<Self, ()> {
        if !cwd.is_dir() {
            return Err(());
        }
        command
            .current_dir(crate::providers::external_path(cwd))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|_| ())?;
        let group = Some(ProcessGroup::register(child.id()));
        let stdin = child.stdin.take().ok_or(())?;
        let stdout = BufReader::new(child.stdout.take().ok_or(())?);
        let mut stderr = child.stderr.take().ok_or(())?;
        let stderr_task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut tail = Vec::new();
            let mut buffer = [0; 2048];
            while let Ok(count) = stderr.read(&mut buffer).await {
                if count == 0 {
                    break;
                }
                tail.extend_from_slice(&buffer[..count]);
                if tail.len() > 16 * 1024 {
                    tail.drain(..tail.len() - 16 * 1024);
                }
            }
            String::from_utf8_lossy(&tail).trim().to_owned()
        });
        Ok(Self {
            child: Some(child),
            group,
            stdin,
            stdout,
            deadline: Instant::now() + timeout,
            bytes: 0,
            frames: 0,
            stderr_task,
            diagnostic: String::new(),
        })
    }

    pub(crate) async fn send(&mut self, message: &Value) -> Result<(), ()> {
        let mut bytes = serde_json::to_vec(message).map_err(|_| ())?;
        bytes.push(b'\n');
        tokio::time::timeout_at(self.deadline, self.stdin.write_all(&bytes))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())
    }

    pub(crate) async fn read(&mut self) -> Result<Value, ()> {
        let deadline = self.deadline;
        tokio::time::timeout_at(deadline, self.read_frame())
            .await
            .map_err(|_| ())?
    }

    pub(crate) fn set_deadline_after(&mut self, duration: Duration) {
        self.deadline = Instant::now() + duration;
    }

    pub(crate) fn timed_out(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub(crate) fn diagnostic(&self) -> &str {
        &self.diagnostic
    }

    async fn read_frame(&mut self) -> Result<Value, ()> {
        let mut frame = Vec::new();
        loop {
            let available = self.stdout.fill_buf().await.map_err(|_| ())?;
            if available.is_empty() {
                return Err(());
            }
            let end = available.iter().position(|byte| *byte == b'\n');
            let consumed = end.map_or(available.len(), |index| index + 1);
            self.bytes += consumed;
            if self.bytes > 4 * 1024 * 1024 || frame.len() + consumed > 512 * 1024 {
                return Err(());
            }
            frame.extend_from_slice(&available[..consumed]);
            self.stdout.consume(consumed);
            if end.is_none() {
                continue;
            }
            self.frames += 1;
            if self.frames > 512 {
                return Err(());
            }
            if frame.iter().all(u8::is_ascii_whitespace) {
                frame.clear();
                continue;
            }
            return serde_json::from_slice(&frame).map_err(|_| ());
        }
    }

    pub(crate) async fn finish(&mut self) {
        self.group.take();
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
            if tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .is_ok()
            {
                self.child.take();
            }
        }
        self.diagnostic = tokio::time::timeout(Duration::from_millis(250), &mut self.stderr_task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        self.stderr_task.abort();
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        self.stderr_task.abort();
        self.group.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
    }
}
