//! Read-only probes: TCP reachability and bounded binary detection.
//!
//! Network probes are explicit and bounded: nothing here phones home
//! unless the caller asked for it (`doctor --ping`), and every probe
//! carries a deadline so a hung network cannot stall the run.

use std::time::Duration;

/// Split an `http(s)://host[:port][/...]` URL into `(host, port)`.
/// Default ports follow the scheme. Returns `None` for anything that
/// does not look like an HTTP URL - the probe treats that as
/// unreachable rather than guessing.
pub fn host_port(url: &str) -> Option<(String, u16)> {
    let after = url.split("://").nth(1)?;
    let hostport = after.split('/').next()?;
    // Strip userinfo if present.
    let hostport = hostport.rsplit('@').next()?;
    if let Some((h, p)) = hostport.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            let host = h.trim_matches(|c| c == '[' || c == ']');
            return Some((host.to_string(), port));
        }
    }
    let port = if url.starts_with("https") { 443 } else { 80 };
    Some((hostport.to_string(), port))
}

/// TCP reachability probe for an HTTP(S) base URL: `true` when something
/// accepts a TCP connection within `timeout`. Plain connect, no TLS, no
/// HTTP request - reachability only, never authentication.
pub fn tcp_probe(url: &str, timeout: Duration) -> bool {
    use std::net::ToSocketAddrs;
    let (host, port) = match host_port(url) {
        Some(hp) => hp,
        None => return false,
    };
    match (host.as_str(), port).to_socket_addrs() {
        Ok(mut addrs) => addrs.any(|a| std::net::TcpStream::connect_timeout(&a, timeout).is_ok()),
        Err(_) => false,
    }
}

/// Run `sh -c <detect_cmd>` with a deadline. A probe that hangs must not
/// stall `doctor`: a `python3 -m pip --version` once took ~9.5s on a cold
/// box, so the timeout is load-bearing, not decorative.
pub fn detect_binary(detect_cmd: &str, timeout: Duration) -> bool {
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(detect_cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

/// Default deadline for binary detection probes.
pub const DETECT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_probe_rejects_garbage_and_refused_ports() {
        // Nothing listens on 1; the probe must answer fast, not hang.
        let start = std::time::Instant::now();
        assert!(!tcp_probe("http://127.0.0.1:1/v1", Duration::from_secs(3)));
        assert!(!tcp_probe("not a url", Duration::from_secs(3)));
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn tcp_probe_sees_a_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        assert!(tcp_probe(
            &format!("http://127.0.0.1:{port}/v1"),
            Duration::from_secs(3)
        ));
    }

    #[test]
    fn host_port_parses_common_shapes() {
        assert_eq!(
            host_port("http://127.0.0.1:11434/v1"),
            Some(("127.0.0.1".to_string(), 11434))
        );
        assert_eq!(
            host_port("https://api.openai.com/v1"),
            Some(("api.openai.com".to_string(), 443))
        );
        assert_eq!(
            host_port("http://example.com"),
            Some(("example.com".to_string(), 80))
        );
        assert_eq!(host_port("not a url"), None);
    }

    #[test]
    fn detect_binary_true_and_false() {
        assert!(detect_binary("command -v sh", Duration::from_secs(5)));
        assert!(!detect_binary(
            "command -v pantheon-definitely-not-a-binary",
            Duration::from_secs(5)
        ));
    }
}
