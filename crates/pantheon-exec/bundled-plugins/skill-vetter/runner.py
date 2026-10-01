#!/usr/bin/env python3
"""skill-vetter runner: static security vetting for plugin/skill installs.

Speaks Pantheon's tool-plugin JSON protocol over stdio:
    stdin  -> {"call_id": "...", "tool": "vet_target", "args": {"path_or_url": "..."}}
    stdout -> {"call_id": "...", "result": {...}}
           or {"call_id": "...", "error": {"code": "...", "cause": "..."}}

Standard library only. This is static analysis over file contents: it
catches known-bad shapes (webhook exfiltration, credential harvesting,
obfuscated payloads, privilege escalation, network listeners) and reports
them with a pass/review/block verdict. It is defense-in-depth guidance,
NOT an enforcement gate and NOT a guarantee: obfuscated or novel
malware can evade pattern checks. Always review flagged code by hand
before installing anything from an untrusted source.
"""

import io
import json
import os
import re
import shutil
import sys
import tarfile
import tempfile
import urllib.parse
import urllib.request
import zipfile

MAX_FETCH_BYTES = 10 * 1024 * 1024
MAX_FILE_BYTES = 2 * 1024 * 1024
MAX_TOTAL_BYTES = 50 * 1024 * 1024
MAX_FILES = 1000
FETCH_TIMEOUT = 20

SKIP_DIRS = {".git", "__pycache__", "node_modules", ".venv", "venv", ".tox", "dist", "build"}

# Hosts considered well-known enough that merely contacting them is not a
# finding on its own. Anything else contacted over http(s) is flagged.
ALLOWLIST_HOSTS = {
    "github.com", "api.github.com", "raw.githubusercontent.com",
    "codeload.github.com", "objects.githubusercontent.com",
    "pypi.org", "files.pythonhosted.org",
    "npmjs.com", "registry.npmjs.org", "registry.yarnpkg.com",
    "crates.io", "static.crates.io",
    "proxy.golang.org", "goproxy.io",
    "unpkg.com", "cdn.jsdelivr.net",
    "api.anthropic.com", "api.openai.com",
    "pypi.python.org",
}
ALLOWLIST_SUFFIXES = (".googleapis.com", ".amazonaws.com", ".cloudfront.net")

# Relay/exfiltration services: seeing traffic to these is a high finding.
RELAY_HOSTS = {
    "discord.com", "discordapp.com",           # /api/webhooks/...
    "hooks.slack.com",
    "webhook.site", "requestbin.com", "requestcatcher.com",
    "pipedream.net", "beeceptor.com", "npoint.io",
    "ngrok.io", "trycloudflare.com",           # inbound tunnels
    "oastify.com", "interactsh.com", "burpcollaborator.net",
}

SECRET_NAME_RE = re.compile(r"(key|token|secret|password|passwd|private|credential|auth)", re.I)
URL_RE = re.compile(r"https?://[^\s'\"<>`]+", re.I)


class VetError(Exception):
    def __init__(self, code, cause):
        super().__init__(cause)
        self.code = code
        self.cause = cause


# ------------------------------------------------------------- target ---

def _validate_url(raw):
    """Accept only http(s) URLs. Returns parsed result or raises VetError."""
    if "://" not in raw and not raw.lower().startswith("http"):
        return None
    try:
        p = urllib.parse.urlparse(raw)
    except Exception:
        raise VetError("VET_BAD_URL", "not a parseable URL: %r" % raw)
    if p.scheme not in ("http", "https"):
        raise VetError(
            "VET_BAD_URL",
            "only http(s) URLs can be vetted, got scheme %r" % p.scheme,
        )
    if p.username or p.password:
        raise VetError("VET_BAD_URL", "refusing URL with embedded credentials")
    if not p.hostname:
        raise VetError("VET_BAD_URL", "URL has no host: %r" % raw)
    return p


def _fetch_url(url):
    req = urllib.request.Request(
        url, headers={"User-Agent": "pantheon-skill-vetter/1.0"}
    )
    try:
        resp = urllib.request.urlopen(req, timeout=FETCH_TIMEOUT)
    except Exception as e:
        raise VetError("VET_FETCH_FAILED", "could not fetch %s: %s" % (url, e))
    final = resp.geturl()
    fp = _validate_url(final)  # re-validate after redirects
    if fp is None:  # pragma: no cover - urlopen only yields http(s)
        raise VetError("VET_BAD_URL", "redirect landed off http(s): %r" % final)
    buf = io.BytesIO()
    total = 0
    while True:
        chunk = resp.read(65536)
        if not chunk:
            break
        total += len(chunk)
        if total > MAX_FETCH_BYTES:
            raise VetError("VET_TOO_LARGE", "download exceeds %d bytes" % MAX_FETCH_BYTES)
        buf.write(chunk)
    return final, buf.getvalue(), resp.headers.get_content_type()


def _safe_extract_zip(data, dest):
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        for info in z.infolist():
            name = info.filename
            if name.startswith("/") or ".." in name.split("/"):
                continue
            target = os.path.join(dest, name)
            if info.is_dir():
                os.makedirs(target, exist_ok=True)
            else:
                os.makedirs(os.path.dirname(target), exist_ok=True)
                with z.open(info) as src, open(target, "wb") as out:
                    shutil.copyfileobj(src, out)


def _safe_extract_tar(data, dest):
    with tarfile.open(fileobj=io.BytesIO(data)) as t:
        for member in t.getmembers():
            name = member.name
            if name.startswith("/") or ".." in name.split("/"):
                continue
            target = os.path.join(dest, name)
            if member.isdir():
                os.makedirs(target, exist_ok=True)
            elif member.isfile():
                os.makedirs(os.path.dirname(target), exist_ok=True)
                src = t.extractfile(member)
                if src is None:
                    continue
                with open(target, "wb") as out:
                    shutil.copyfileobj(src, out)


def _materialize_target(path_or_url):
    """Return (label, dir_to_scan, tempdir_or_None)."""
    if not isinstance(path_or_url, str) or not path_or_url.strip():
        raise VetError("VET_BAD_ARGS", "path_or_url must be a non-empty string")
    target = path_or_url.strip()

    parsed = _validate_url(target)
    if parsed is not None:
        if target.lower().rstrip("/").endswith(".git"):
            raise VetError(
                "VET_BAD_URL",
                "git repository URLs are not fetched: clone it yourself, then vet the local directory",
            )
        final_url, data, ctype = _fetch_url(target)
        tmp = tempfile.mkdtemp(prefix="skill-vetter-")
        low = final_url.lower()
        try:
            if low.endswith(".zip") or ctype == "application/zip":
                _safe_extract_zip(data, tmp)
            elif low.endswith((".tar.gz", ".tgz", ".tar")) or ctype in (
                "application/gzip", "application/x-tar", "application/x-gzip"):
                _safe_extract_tar(data, tmp)
            else:
                # Single file.
                name = os.path.basename(urllib.parse.urlparse(final_url).path) or "downloaded"
                with open(os.path.join(tmp, name), "wb") as f:
                    f.write(data)
        except VetError:
            raise
        except Exception as e:
            shutil.rmtree(tmp, ignore_errors=True)
            raise VetError("VET_EXTRACT_FAILED", "could not unpack download: %s" % e)
        return final_url, tmp, tmp

    # Local path.
    path = os.path.expanduser(target)
    if not os.path.isabs(path):
        raise VetError("VET_BAD_PATH", "local path must be absolute, got %r" % target)
    if not os.path.exists(path):
        raise VetError("VET_NOT_FOUND", "no such file or directory: %s" % path)
    if os.path.isfile(path):
        low = path.lower()
        if low.endswith(".zip") or low.endswith((".tar.gz", ".tgz", ".tar")):
            tmp = tempfile.mkdtemp(prefix="skill-vetter-")
            with open(path, "rb") as f:
                data = f.read(MAX_FETCH_BYTES + 1)
            if len(data) > MAX_FETCH_BYTES:
                raise VetError("VET_TOO_LARGE", "archive exceeds %d bytes" % MAX_FETCH_BYTES)
            try:
                if low.endswith(".zip"):
                    _safe_extract_zip(data, tmp)
                else:
                    _safe_extract_tar(data, tmp)
            except Exception as e:
                shutil.rmtree(tmp, ignore_errors=True)
                raise VetError("VET_EXTRACT_FAILED", "could not unpack archive: %s" % e)
            return path, tmp, tmp
        # Single local file: scan it in place via a temp dir with a link.
        tmp = tempfile.mkdtemp(prefix="skill-vetter-")
        shutil.copy2(path, os.path.join(tmp, os.path.basename(path)))
        return path, tmp, tmp
    return path, path, None


def _iter_files(root):
    files, skipped = [], 0
    total = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS and not d.startswith(".")]
        for fn in filenames:
            if len(files) >= MAX_FILES:
                skipped += 1
                continue
            fp = os.path.join(dirpath, fn)
            try:
                size = os.path.getsize(fp)
            except OSError:
                skipped += 1
                continue
            if size > MAX_FILE_BYTES or total + size > MAX_TOTAL_BYTES:
                skipped += 1
                continue
            total += size
            files.append(fp)
    return files, skipped


def _read_text(fp):
    try:
        with open(fp, "rb") as f:
            head = f.read(8192)
            if b"\x00" in head:
                return None  # binary
            f.seek(0)
            return f.read(MAX_FILE_BYTES + 1).decode("utf-8", errors="replace")
    except OSError:
        return None


# ---------------------------------------------------------------- checks ---

def _host_of(url):
    try:
        return urllib.parse.urlparse(url).hostname or ""
    except Exception:
        return ""


def _check_urls(rel, text, findings, hosts_seen):
    for m in URL_RE.finditer(text):
        url = m.group(0).rstrip(".,;:)]}")
        host = _host_of(url).lower()
        if not host:
            continue
        hosts_seen.add(host)
        line = text.count("\n", 0, m.start()) + 1
        if host in RELAY_HOSTS or any(host.endswith("." + r) or host == r for r in RELAY_HOSTS):
            findings.append({
                "severity": "high", "check": "webhook-exfil",
                "file": rel, "line": line,
                "detail": "reference to relay/exfiltration host %r" % host,
            })
        elif "webhook" in url.lower():
            findings.append({
                "severity": "medium", "check": "webhook-exfil",
                "file": rel, "line": line,
                "detail": "URL contains 'webhook' on non-relay host %r" % host,
            })
        elif (host not in ALLOWLIST_HOSTS
                and not host.endswith(ALLOWLIST_SUFFIXES)
                and host not in ("localhost", "127.0.0.1", "::1")
                and not host.startswith("192.168.") and not host.startswith("10.")):
            findings.append({
                "severity": "medium", "check": "unknown-host",
                "file": rel, "line": line,
                "detail": "contacts unknown external host %r" % host,
            })


CRED_FILE_RES = [
    (re.compile(r"~/\.(ssh|aws|gnupg|pki)\b"), "credential directory"),
    (re.compile(r"\.ssh/(id_rsa|id_ed25519|id_ecdsa|config)\b"), "SSH private key/config"),
    (re.compile(r"id_rsa\b"), "SSH private key file"),
    (re.compile(r"['\"][^'\"]*\.pem['\"]"), "PEM private key file"),
    (re.compile(r"\.netrc\b"), "netrc credentials file"),
    (re.compile(r"~/\.aws/credentials"), "AWS credentials file"),
]


def _check_credentials(rel, text, findings):
    for i, line in enumerate(text.splitlines(), 1):
        # Environment harvesting.
        if re.search(r"os\.environ\b|os\.getenv|process\.env|System\.getenv|getenv\s*\(|\bprintenv\b", line):
            if re.search(r"list\s*\(\s*os\.environ|os\.environ\.copy|Object\.keys\s*\(\s*process\.env|for\s+\w+\s+in\s+os\.environ", line):
                findings.append({
                    "severity": "medium", "check": "cred-env-bulk",
                    "file": rel, "line": i,
                    "detail": "bulk environment read: %s" % line.strip()[:120],
                })
            else:
                names = SECRET_NAME_RE.findall(line)
                findings.append({
                    "severity": "high" if names else "medium",
                    "check": "cred-env",
                    "file": rel, "line": i,
                    "detail": ("reads secret-named env var (%s)" % ", ".join(sorted(set(names))))
                    if names else "reads environment variable: %s" % line.strip()[:120],
                })
        # Credential files.
        for rx, what in CRED_FILE_RES:
            if rx.search(line):
                findings.append({
                    "severity": "high", "check": "cred-file",
                    "file": rel, "line": i,
                    "detail": "references %s: %s" % (what, line.strip()[:120]),
                })
                break
        else:
            if re.search(r"['\"]\.env['\"]|load_dotenv|dotenv", line):
                findings.append({
                    "severity": "medium", "check": "cred-file",
                    "file": rel, "line": i,
                    "detail": "reads .env file: %s" % line.strip()[:120],
                })


BLOB_RE = re.compile(r"[A-Za-z0-9+/]{200,}={0,2}")
HEX_RE = re.compile(r"(?:0x)?[0-9a-fA-F]{400,}")
DYNEXEC_RE = re.compile(r"\b(exec|eval|compile)\s*\(")


def _check_obfuscation(rel, text, findings):
    lines = text.splitlines()
    has_dynexec = bool(DYNEXEC_RE.search(text))
    has_decode = bool(re.search(r"b64decode|base64\.(b64decode|decodebytes)|fromhex|unhexlify", text))
    for i, line in enumerate(lines, 1):
        if BLOB_RE.search(line) or HEX_RE.search(line):
            findings.append({
                "severity": "high" if (has_dynexec and has_decode) else "medium",
                "check": "obfuscated-blob",
                "file": rel, "line": i,
                "detail": ("long encoded blob paired with decode+dynamic-exec in this file"
                            if (has_dynexec and has_decode)
                            else "long base64/hex blob (possible packed payload)"),
            })
            break  # one per file is enough signal


REMOTE_EXEC_RES = [
    (re.compile(r"\bexec\s*\(\s*(requests|urllib|urlopen|http)\b"), "exec() on fetched remote code"),
    (re.compile(r"\beval\s*\(\s*(requests|urllib|urlopen|http)\b"), "eval() on fetched remote code"),
    (re.compile(r"curl[^|]*\|\s*(sh|bash)\b"), "curl piped to shell"),
    (re.compile(r"wget[^|]*\|\s*(sh|bash)\b"), "wget piped to shell"),
    (re.compile(r"Invoke-Expression|IEX\s*\(.*http", re.I), "PowerShell IEX on remote script"),
]


def _check_remote_exec(rel, text, findings):
    for i, line in enumerate(text.splitlines(), 1):
        for rx, what in REMOTE_EXEC_RES:
            if rx.search(line):
                findings.append({
                    "severity": "high", "check": "remote-code-exec",
                    "file": rel, "line": i,
                    "detail": "%s: %s" % (what, line.strip()[:140]),
                })
                break
        else:
            if DYNEXEC_RE.search(line) and not re.search(r"exec\s*\(\s*[\"']", line):
                findings.append({
                    "severity": "medium", "check": "dynamic-exec",
                    "file": rel, "line": i,
                    "detail": "dynamic code execution: %s" % line.strip()[:120],
                })


PRIV_RE = re.compile(r"\bsudo\b|\bpkexec\b|\bdoas\b|\brunas\b|os\.setuid|os\.setgid|os\.seteuid|chmod\s+(\+s|4755|0o4755)|S_ISUID")


def _check_privilege(rel, text, findings):
    for i, line in enumerate(text.splitlines(), 1):
        m = PRIV_RE.search(line)
        if m:
            findings.append({
                "severity": "high", "check": "priv-escalation",
                "file": rel, "line": i,
                "detail": "privilege escalation primitive %r: %s" % (m.group(0), line.strip()[:120]),
            })


LISTEN_RES = [
    (re.compile(r"\.listen\s*\("), "socket listen()"),
    (re.compile(r"\.bind\s*\("), "socket bind()"),
    (re.compile(r"http\.server|SimpleHTTPServer|BaseHTTPServer"), "stdlib HTTP server"),
    (re.compile(r"app\.run\s*\("), "web framework app.run()"),
    (re.compile(r"uvicorn\.run"), "uvicorn server"),
    (re.compile(r"0\.0\.0\.0"), "bind to all interfaces"),
]


def _check_listeners(rel, text, findings):
    for i, line in enumerate(text.splitlines(), 1):
        for rx, what in LISTEN_RES:
            if rx.search(line):
                findings.append({
                    "severity": "high", "check": "net-listener",
                    "file": rel, "line": i,
                    "detail": "%s: %s" % (what, line.strip()[:120]),
                })
                break


def vet_target(path_or_url):
    label, scan_dir, tmp = _materialize_target(path_or_url)
    try:
        files, skipped = _iter_files(scan_dir)
        findings = []
        hosts_seen = set()
        for fp in files:
            rel = os.path.relpath(fp, scan_dir)
            text = _read_text(fp)
            if text is None:
                skipped += 1
                continue
            _check_urls(rel, text, findings, hosts_seen)
            _check_credentials(rel, text, findings)
            _check_obfuscation(rel, text, findings)
            _check_remote_exec(rel, text, findings)
            _check_privilege(rel, text, findings)
            _check_listeners(rel, text, findings)

        by_sev = {"high": 0, "medium": 0, "low": 0}
        for f in findings:
            by_sev[f["severity"]] = by_sev.get(f["severity"], 0) + 1
        if by_sev["high"]:
            verdict, reason = "block", "%d high-severity finding(s)" % by_sev["high"]
        elif by_sev["medium"]:
            verdict, reason = "review", "%d medium-severity finding(s), no high" % by_sev["medium"]
        else:
            verdict, reason = "pass", "no high or medium findings"

        findings.sort(key=lambda f: ({"high": 0, "medium": 1, "low": 2}[f["severity"]], f["file"], f["line"]))
        return {
            "target": label,
            "verdict": verdict,
            "verdict_reason": reason,
            "findings": findings,
            "summary": {
                "files_scanned": len(files),
                "files_skipped": skipped,
                "findings_by_severity": by_sev,
                "external_hosts": sorted(hosts_seen),
            },
            "notes": [
                "Static pattern checks only: obfuscated or novel malware can evade them.",
                "This report is defense-in-depth guidance, not an enforcement gate.",
                "Review flagged code by hand before installing from untrusted sources.",
            ],
        }
    finally:
        if tmp:
            shutil.rmtree(tmp, ignore_errors=True)


TOOLS = {"vet_target": vet_target}


def handle_request(req):
    call_id = req.get("call_id", "unknown")
    tool = req.get("tool")
    args = req.get("args") or {}
    if not isinstance(args, dict):
        return {"call_id": call_id,
                "error": {"code": "VET_BAD_ARGS", "cause": "args must be an object"}}
    fn = TOOLS.get(tool)
    if fn is None:
        return {"call_id": call_id,
                "error": {"code": "VET_UNKNOWN_TOOL", "cause": "unknown tool %r" % (tool,)}}
    try:
        return {"call_id": call_id, "result": fn(args.get("path_or_url"))}
    except VetError as e:
        return {"call_id": call_id, "error": {"code": e.code, "cause": e.cause}}
    except Exception as e:
        return {"call_id": call_id,
                "error": {"code": "VET_INTERNAL", "cause": "%s: %s" % (type(e).__name__, e)}}


def main():
    out = sys.stdout
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            out.write(json.dumps({"call_id": "unknown",
                                  "error": {"code": "VET_BAD_ARGS",
                                            "cause": "request is not valid JSON: %s" % e}}) + "\n")
            out.flush()
            continue
        if not isinstance(req, dict):
            out.write(json.dumps({"call_id": "unknown",
                                  "error": {"code": "VET_BAD_ARGS",
                                            "cause": "request must be a JSON object"}}) + "\n")
            out.flush()
            continue
        out.write(json.dumps(handle_request(req), default=str) + "\n")
        out.flush()


if __name__ == "__main__":
    main()
