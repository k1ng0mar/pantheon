#!/usr/bin/env python3
"""Pantheon Camoufox backend shim: long-lived JSON-over-stdio driver.

Protocol
--------
* argv[1] is the launch-config JSON (headless/os/humanize/geoip/proxy/
  locale/timezone/block_images/block_webrtc/fingerprint_preset).
* After launching, the shim prints one handshake line:
      {"ready": true}
  or, when the `camoufox` package (or its fetched browser binary) is
  missing:
      {"ready": false, "error": "<human text>", "kind": "missing"}
  and exits non-zero. Pantheon maps `kind == "missing"` to a clean
  "backend unavailable" error with install instructions, never a crash.
* Then it reads one JSON command object per stdin line and writes one
  JSON response object per stdout line:
      command:  {"argv": ["navigate", "https://example.com"]}
      response: {"ok": true, "result": {...}}
                {"ok": false, "error": "...", "stale": true}

Command vocabulary mirrors Pantheon's canonical browser argv (the same
vocabulary the gsd-browser / playwright-cli backends speak), so the
Rust side passes most commands through unchanged:

  navigate <url> | back | forward | reload | snapshot
  click-ref <ref> | hover-ref <ref> | fill-ref <ref> <text>
  click <selector> | type <selector> <text> | press <key>
  eval <js> | screenshot [--output P] [--format F] | page-source
  close

`snapshot` returns {"version": N, "refs": {"e0": {...}, ...}, "count": N}
with gsd-style `@vN:eM` refs. Refs resolve to live Playwright element
handles held by this process; any navigation bumps the version and
clears them, and resolving a stale ref yields {"stale": true}.

Requires: pip install "camoufox[geoip]" && python -m camoufox fetch
"""

import json
import sys
import traceback

# Candidates for the snapshot enumeration. Keep the list small: every
# handle costs an evaluate round-trip.
VIS_SELECTOR = (
    "a,button,input,select,textarea,summary,label,"
    "[role=button],[role=link],[role=checkbox],[role=radio],"
    "[role=switch],[role=tab],[role=menuitem],[onclick]"
)

# Bound the snapshot so a pathological page cannot flood the model.
MAX_SNAPSHOT_ELEMENTS = 200

INFO_JS = """el => {
  const r = el.getBoundingClientRect();
  const cs = getComputedStyle(el);
  const visible = r.width > 0 && r.height > 0
    && cs.visibility !== 'hidden' && cs.display !== 'none';
  const name = (el.innerText || el.value || el.getAttribute('aria-label')
    || el.getAttribute('title') || el.getAttribute('alt')
    || el.getAttribute('placeholder') || '').trim().slice(0, 80);
  return {
    visible,
    name,
    role: el.getAttribute('role') || el.tagName.toLowerCase(),
    type: el.getAttribute('type') || '',
  };
}"""


def respond(payload):
    sys.stdout.write(json.dumps(payload, default=str) + "\n")
    sys.stdout.flush()


def fail(message, stale=False):
    out = {"ok": false, "error": message}
    if stale:
        out["stale"] = True
    respond(out)


def flag(argv, name):
    try:
        i = argv.index(name)
    except ValueError:
        return None
    return argv[i + 1] if i + 1 < len(argv) else None


class Shim:
    def __init__(self, page):
        self.page = page
        self.version = 0
        self.refs = {}  # "@vN:eM" -> element handle

    def bump(self):
        self.version += 1
        self.refs = {}

    def snapshot(self):
        self.bump()
        refs = {}
        try:
            handles = self.page.query_selector_all(VIS_SELECTOR)
        except Exception as e:  # noqa: BLE001 - surfaced as command error
            return {"ok": False, "error": f"snapshot failed: {e}"}
        n = 0
        for h in handles:
            if n >= MAX_SNAPSHOT_ELEMENTS:
                break
            try:
                info = h.evaluate(INFO_JS)
            except Exception:  # noqa: BLE001 - stale/detached handle
                continue
            if not info.get("visible"):
                continue
            ref = f"@v{self.version}:e{n}"
            self.refs[ref] = h
            refs[f"e{n}"] = {
                "ref": ref,
                "role": info.get("role", ""),
                "name": info.get("name", ""),
                "type": info.get("type", ""),
            }
            n += 1
        return {
            "ok": True,
            "result": {"version": self.version, "refs": refs, "count": n},
        }

    def resolve(self, ref):
        handle = self.refs.get(ref)
        if handle is None:
            v = ref.split(":")[0] if ref.startswith("@v") else "?"
            return None, (
                f"ref {ref} is stale (snapshot v{v}, current v{self.version}): "
                "take a fresh snapshot"
            )
        return handle, None

    def run(self, argv):
        cmd = argv[0] if argv else ""
        page = self.page
        try:
            if cmd == "navigate":
                page.goto(argv[1], wait_until="domcontentloaded", timeout=30000)
                self.bump()
                return {"ok": True, "result": {"title": page.title(), "url": page.url}}
            if cmd == "back":
                page.go_back(wait_until="domcontentloaded", timeout=30000)
                self.bump()
                return {"ok": True, "result": {"url": page.url}}
            if cmd == "forward":
                page.go_forward(wait_until="domcontentloaded", timeout=30000)
                self.bump()
                return {"ok": True, "result": {"url": page.url}}
            if cmd == "reload":
                page.reload(wait_until="domcontentloaded", timeout=30000)
                self.bump()
                return {"ok": True, "result": {"url": page.url}}
            if cmd == "snapshot":
                return self.snapshot()
            if cmd in ("click-ref", "hover-ref", "fill-ref"):
                handle, err = self.resolve(argv[1] if len(argv) > 1 else "")
                if err:
                    return {"ok": False, "error": err, "stale": True}
                if cmd == "click-ref":
                    handle.click(timeout=15000)
                elif cmd == "hover-ref":
                    handle.hover(timeout=15000)
                else:
                    handle.fill(argv[2] if len(argv) > 2 else "", timeout=15000)
                return {"ok": True, "result": {"ok": True}}
            if cmd == "click":
                page.click(argv[1], timeout=15000)
                return {"ok": True, "result": {"ok": True}}
            if cmd == "type":
                page.fill(argv[1], argv[2] if len(argv) > 2 else "", timeout=15000)
                return {"ok": True, "result": {"ok": True}}
            if cmd == "press":
                page.keyboard.press(argv[1] if len(argv) > 1 else "Enter")
                return {"ok": True, "result": {"ok": True}}
            if cmd == "eval":
                result = page.evaluate(argv[1] if len(argv) > 1 else "null")
                return {"ok": True, "result": result}
            if cmd == "screenshot":
                path = flag(argv, "--output") or "screenshot.png"
                fmt = (flag(argv, "--format") or "png").lower()
                # Playwright-Python only supports png/jpeg.
                shot_type = "jpeg" if fmt in ("jpeg", "jpg") else "png"
                page.screenshot(path=path, type=shot_type)
                return {"ok": True, "result": {"path": path, "format": shot_type}}
            if cmd == "page-source":
                return {"ok": True, "result": {"html": page.content()}}
            if cmd == "close":
                return {"ok": True, "result": {"ok": True}, "close": True}
            return {"ok": False, "error": f"unsupported command: {cmd!r}"}
        except Exception as e:  # noqa: BLE001 - every failure is a response
            return {"ok": False, "error": f"{cmd} failed: {e}"}


def launch_kwargs(cfg):
    kwargs = {}
    headless = cfg.get("headless", True)
    # "virtual" = Xvfb wrapper on Linux (needs xvfb installed).
    kwargs["headless"] = headless
    if cfg.get("os"):
        kwargs["os"] = cfg["os"]
    humanize = cfg.get("humanize")
    if humanize is not None:
        kwargs["humanize"] = humanize if humanize is True else float(humanize or 0) or True
    if cfg.get("geoip"):
        kwargs["geoip"] = True
    if cfg.get("locale"):
        kwargs["locale"] = cfg["locale"]
    if cfg.get("timezone"):
        kwargs["timezone"] = cfg["timezone"]
    proxy = cfg.get("proxy")
    if proxy and proxy.get("server"):
        p = {"server": proxy["server"]}
        if proxy.get("username"):
            p["username"] = proxy["username"]
        if proxy.get("password"):
            p["password"] = proxy["password"]
        kwargs["proxy"] = p
    if cfg.get("block_images"):
        kwargs["block_images"] = True
    if cfg.get("block_webrtc"):
        kwargs["block_webrtc"] = True
    if cfg.get("fingerprint_preset", True):
        kwargs["fingerprint_preset"] = True
    return kwargs


def missing_exit(message):
    respond({"ready": False, "error": message, "kind": "missing"})
    sys.exit(3)


def main():
    try:
        cfg = json.loads(sys.argv[1]) if len(sys.argv) > 1 else {}
    except Exception as e:  # noqa: BLE001 - bad argv is fatal
        sys.stderr.write(f"camofox shim: bad launch config: {e}\n")
        sys.exit(2)

    try:
        from camoufox.sync_api import Camoufox
    except ImportError:
        missing_exit(
            "the `camoufox` Python package is not installed. "
            'Install it with `pip install "camoufox[geoip]"` '
            "then fetch the browser with `python -m camoufox fetch`."
        )

    try:
        launcher = Camoufox(**launch_kwargs(cfg))
        browser = launcher.__enter__()
    except Exception as e:  # noqa: BLE001 - launch failure text decides kind
        text = f"{type(e).__name__}: {e}"
        if "fetch" in text.lower() or "not found" in text.lower():
            missing_exit(
                "Camoufox's browser binary is missing. "
                "Run `python -m camoufox fetch` to download it (~150MB). "
                f"Detail: {text}"
            )
        sys.stderr.write(f"camofox shim: launch failed: {text}\n")
        traceback.print_exc()
        sys.exit(2)

    try:
        page = browser.new_page()
    except Exception as e:  # noqa: BLE001
        sys.stderr.write(f"camofox shim: new_page failed: {e}\n")
        sys.exit(2)

    respond({"ready": True})
    shim = Shim(page)
    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                cmd = json.loads(line)
            except json.JSONDecodeError as e:
                fail(f"bad command JSON: {e}")
                continue
            argv = cmd.get("argv", [])
            if not isinstance(argv, list):
                fail("command must carry an 'argv' list")
                continue
            try:
                out = shim.run([str(a) for a in argv])
            except Exception as e:  # noqa: BLE001 - never let one command kill the loop
                fail(f"command crashed: {e}")
                continue
            respond({"ok": out.get("ok", False),
                     **{k: v for k, v in out.items() if k != "ok"}})
            if out.get("close"):
                break
    finally:
        try:
            launcher.__exit__(None, None, None)
        except Exception:  # noqa: BLE001 - best effort on shutdown
            pass


if __name__ == "__main__":
    main()
