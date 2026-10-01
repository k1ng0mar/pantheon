"""Unit tests for hermes-web-search-plus. Stdlib only; run with:
    python3 tests/test_web_search_plus.py

Provider HTTP is monkeypatched — no network is used. The SSRF-guard
tests resolve only literal/local names and refuse before connecting.
"""

import importlib.util
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGIN = os.path.dirname(HERE)
RUN_PY = os.path.join(PLUGIN, "run.py")

spec = importlib.util.spec_from_file_location("wsp_run", RUN_PY)
wsp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wsp)


def test_module_imports_with_stdlib_only():
    assert hasattr(wsp, "tool_web_search")
    assert hasattr(wsp, "tool_extract_page")
    assert wsp._TOOLS == {"web_search": wsp.tool_web_search,
                         "extract_page": wsp.tool_extract_page}


def test_serper_shape_parsed():
    wsp._post_json_orig = wsp._post_json
    wsp._post_json = lambda url, headers, body, timeout=20: {
        "organic": [{"title": "T", "link": "https://e.com/x",
                     "snippet": "S"}]}
    try:
        os.environ["SERPER_API_KEY"] = "k"
        out = wsp.tool_web_search({"query": "q", "num": 5,
                                   "provider": "serper"})
    finally:
        wsp._post_json = wsp._post_json_orig
        del os.environ["SERPER_API_KEY"]
    assert out["provider"] == "serper"
    assert out["results"] == [{"title": "T", "url": "https://e.com/x",
                               "snippet": "S"}], out
    assert out["untrusted_notice"].startswith("UNTRUSTED CONTENT")


def test_serpbase_requires_status_zero():
    wsp._post_json_orig = wsp._post_json
    wsp._post_json = lambda url, headers, body, timeout=20: {"status": 7}
    try:
        os.environ["SERPBASE_API_KEY"] = "k"
        try:
            wsp.tool_web_search({"query": "q", "provider": "serpbase"})
        except wsp.ProviderError as e:
            assert "status" in str(e)
        else:
            raise AssertionError("expected ProviderError")
    finally:
        wsp._post_json = wsp._post_json_orig
        del os.environ["SERPBASE_API_KEY"]


def test_linkup_uses_results_or_sources():
    seen = {}

    def fake_post(url, headers, body, timeout=20):
        seen["auth"] = headers.get("Authorization")
        seen["body"] = body
        return {"results": [{"name": "N", "url": "https://l.com",
                             "content": "C"}]}
    wsp._post_json_orig = wsp._post_json
    wsp._post_json = fake_post
    try:
        os.environ["LINKUP_API_KEY"] = "k"
        out = wsp.tool_web_search({"query": "q", "provider": "linkup"})
    finally:
        wsp._post_json = wsp._post_json_orig
        del os.environ["LINKUP_API_KEY"]
    assert seen["auth"] == "Bearer k"
    assert seen["body"]["outputType"] == "searchResults"
    assert out["results"][0]["title"] == "N"


def test_firecrawl_search_shape():
    wsp._post_json_orig = wsp._post_json
    wsp._post_json = lambda url, headers, body, timeout=20: {
        "success": True,
        "data": {"web": [{"title": "T", "url": "https://f.com",
                          "description": "D"}]}}
    try:
        os.environ["FIRECRAWL_API_KEY"] = "k"
        out = wsp.tool_web_search({"query": "q", "provider": "firecrawl"})
    finally:
        wsp._post_json = wsp._post_json_orig
        del os.environ["FIRECRAWL_API_KEY"]
    assert out["results"] == [{"title": "T", "url": "https://f.com",
                               "snippet": "D"}], out


def test_you_search_shape():
    wsp._get_json_orig = wsp._get_json
    wsp._get_json = lambda url, headers, timeout=20: {
        "results": {"web": [{"title": "T", "url": "https://y.com",
                             "snippets": ["S1", "S2"]}]}}
    try:
        os.environ["YOU_API_KEY"] = "k"
        out = wsp.tool_web_search({"query": "q", "provider": "you"})
    finally:
        wsp._get_json = wsp._get_json_orig
        del os.environ["YOU_API_KEY"]
    assert out["results"][0]["snippet"] == "S1", out


def test_keenable_keyless_public_endpoint():
    seen = {}

    def fake_post(url, headers, body, timeout=20):
        seen["url"] = url
        seen["headers"] = headers
        return {"results": [{"title": "T", "url": "https://k.com",
                             "snippet": "S"}]}
    wsp._post_json_orig = wsp._post_json
    wsp._post_json = fake_post
    try:
        os.environ.pop("KEENABLE_API_KEY", None)
        out = wsp.tool_web_search({"query": "q", "provider": "keenable"})
    finally:
        wsp._post_json = wsp._post_json_orig
    assert seen["url"] == "https://api.keenable.ai/v1/search/public", seen
    assert "X-Keenable-Title" in seen["headers"]
    assert "X-API-Key" not in seen["headers"]
    assert out["provider"] == "keenable"


def test_unknown_provider_rejected():
    try:
        wsp.tool_web_search({"query": "q", "provider": "nope"})
    except wsp.ProviderError as e:
        assert "unknown provider" in str(e)
    else:
        raise AssertionError("expected ProviderError")


def test_unconfigured_forced_provider_errors():
    os.environ.pop("SERPER_API_KEY", None)
    try:
        wsp.tool_web_search({"query": "q", "provider": "serper"})
    except wsp.ProviderError as e:
        assert "not configured" in str(e)
    else:
        raise AssertionError("expected ProviderError")


def test_failover_skips_failed_provider():
    calls = []

    def fake_post(url, headers, body, timeout=20):
        calls.append(url)
        if "serper" in url:
            raise wsp.ProviderError("provider HTTP error 401")
        return {"status": 0, "organic": []}
    wsp._post_json_orig = wsp._post_json
    wsp._post_json = fake_post
    try:
        os.environ["SERPER_API_KEY"] = "k1"
        os.environ["SERPBASE_API_KEY"] = "k2"
        out = wsp.tool_web_search({"query": "q"})
    finally:
        wsp._post_json = wsp._post_json_orig
        del os.environ["SERPER_API_KEY"]
        del os.environ["SERPBASE_API_KEY"]
    assert out["provider"] == "serpbase", out
    assert any("serper" in u for u in calls)


def test_keys_never_leak_into_errors():
    os.environ["SERPER_API_KEY"] = "SECRET-KEY-ABC123"

    def fake_post(url, headers, body, timeout=20):
        raise wsp.ProviderError("provider HTTP error 401")
    wsp._post_json_orig = wsp._post_json
    wsp._post_json = fake_post
    try:
        try:
            wsp.tool_web_search({"query": "q", "provider": "serper"})
        except wsp.ProviderError as e:
            assert "SECRET-KEY-ABC123" not in str(e)
        else:
            raise AssertionError("expected ProviderError")
    finally:
        wsp._post_json = wsp._post_json_orig
        del os.environ["SERPER_API_KEY"]


def test_extract_tavily_shape():
    wsp._post_json_orig = wsp._post_json
    wsp._post_json = lambda url, headers, body, timeout=20: {
        "results": [{"title": "T", "raw_content": "BODY"}]}
    try:
        os.environ["TAVILY_API_KEY"] = "k"
        out = wsp.tool_extract_page({"url": "https://e.com/x",
                                     "provider": "tavily"})
    finally:
        wsp._post_json = wsp._post_json_orig
        del os.environ["TAVILY_API_KEY"]
    assert out["title"] == "T" and out["text"] == "BODY", out
    assert out["untrusted_notice"].startswith("UNTRUSTED CONTENT")


def test_extract_caps_text():
    wsp._post_json_orig = wsp._post_json
    wsp._post_json = lambda url, headers, body, timeout=20: {
        "results": [{"raw_content": "x" * 5000}]}
    try:
        os.environ["TAVILY_API_KEY"] = "k"
        out = wsp.tool_extract_page({"url": "https://e.com/x",
                                     "provider": "tavily", "max_chars": 100})
    finally:
        wsp._post_json = wsp._post_json_orig
        del os.environ["TAVILY_API_KEY"]
    assert len(out["text"]) == 100


# --- SSRF guard ------------------------------------------------------------

def test_validate_url_rejects_bad_schemes():
    for bad in ["ftp://e.com/x", "file:///etc/passwd", "gopher://e.com",
                "javascript:alert(1)", "data:text/plain,hi"]:
        try:
            wsp._validate_url(bad)
        except wsp.ProviderError:
            pass
        else:
            raise AssertionError("accepted %r" % bad)


def test_validate_url_rejects_userinfo_and_tricks():
    for bad in ["http://user:pass@e.com/", "http://e.com\\@evil.com/",
                "http://e.com/x y", "http://e.com/\x00x"]:
        try:
            wsp._validate_url(bad)
        except wsp.ProviderError:
            pass
        else:
            raise AssertionError("accepted %r" % bad)


def test_validate_url_accepts_plain_https():
    parts = wsp._validate_url("https://example.com:8443/a?b=c")
    assert parts.scheme == "https" and parts.hostname == "example.com"


def test_check_host_global_rejects_loopback():
    for host in ["127.0.0.1", "localhost", "::1", "10.0.0.1",
                 "192.168.1.1"]:
        try:
            wsp._check_host_global(host)
        except wsp.ProviderError:
            pass
        else:
            raise AssertionError("accepted %r" % host)


def test_direct_fetch_refuses_private_before_connect():
    try:
        wsp._direct_fetch("http://127.0.0.1/", 1000)
    except wsp.ProviderError as e:
        assert "non-public" in str(e), e
    else:
        raise AssertionError("expected ProviderError")


# --- protocol loop ---------------------------------------------------------

def test_protocol_loop_roundtrip():
    # Success path uses a data: URL-free local check: extract via the
    # "direct" provider against a loopback URL fails in the SSRF guard
    # before connecting — no network, but the full loop runs.
    lines = [
        json.dumps({"call_id": "c1", "tool": "extract_page",
                    "args": {"url": "http://127.0.0.1/",
                             "provider": "direct"}}),
        json.dumps({"call_id": "c2", "tool": "web_search",
                    "args": {"query": "q", "provider": "serper"}}),
        "not json at all",
    ]
    env = dict(os.environ)
    env.pop("SERPER_API_KEY", None)
    proc = subprocess.run(
        [sys.executable, RUN_PY], input="\n".join(lines) + "\n",
        capture_output=True, text=True, timeout=30, env=env)
    assert proc.returncode == 0, proc.stderr
    out_lines = [ln for ln in proc.stdout.splitlines() if ln.strip()]
    assert len(out_lines) == 2, out_lines  # garbage line ignored
    r1 = json.loads(out_lines[0])
    assert r1["call_id"] == "c1"
    assert r1["error"]["code"] == "provider_error"
    assert "non-public" in r1["error"]["cause"], r1
    r2 = json.loads(out_lines[1])
    assert r2["call_id"] == "c2"
    assert "not configured" in r2["error"]["cause"], r2


def test_protocol_loop_unknown_tool_errors():
    inp = json.dumps({"call_id": "c2", "tool": "nope",
                      "args": {}}) + "\n"
    proc = subprocess.run(
        [sys.executable, RUN_PY], input=inp, capture_output=True,
        text=True, timeout=30)
    assert proc.returncode == 0, proc.stderr
    resp = json.loads(proc.stdout.strip())
    assert resp["call_id"] == "c2"
    assert resp["error"]["code"] == "provider_error", resp


if __name__ == "__main__":
    this = sys.modules[__name__]
    tests = [(k, v) for k, v in sorted(vars(this).items())
             if k.startswith("test_") and callable(v)]
    failed = 0
    for name, fn in tests:
        try:
            fn()
        except Exception as e:
            failed += 1
            print("FAIL %s: %r" % (name, e))
        else:
            print("ok   %s" % name)
    print("%d/%d passed" % (len(tests) - failed, len(tests)))
    if failed:
        sys.exit(1)
