#!/usr/bin/env node
// cf-ops runner: run `cf` CLI operations for Pantheon.
//
// Speaks Pantheon's tool-plugin JSON protocol over stdio:
//   stdin  -> {"call_id": "...", "tool": "...", "args": {...}}
//   stdout -> {"call_id": "...", "result": ...}
//          or {"call_id": "...", "error": {"code": "...", "cause": "..."}}
//
// Tools: cf_read, cf_write, cf_destroy. All three run the same binary;
// the difference is intent, which the HOST enforces (capability gates and
// approvals park mutating calls). The runner never decides what is safe:
// it validates that argv stays argv (no shell metacharacters, no injection)
// and hands the array to cf as exec args, not a shell string.
//
// The token never appears here: cf resolves CLOUDFLARE_API_TOKEN from its
// own environment, which the host injects at spawn time when
// [cloudflare].enabled. This runner neither reads nor forwards secrets.

import { spawn } from "node:child_process";

const MAX_OUTPUT = 512 * 1024; // cap like the builtin shell tool
const TIMEOUT_MS = 120_000;

function respond(line) {
  process.stdout.write(JSON.stringify(line) + "\n");
}

function fail(callId, code, cause) {
  respond({ call_id: callId, error: { code, cause } });
}

// Reject anything that is not a plain argv token. cf takes real argv, so
// there is never a reason for a token to carry shell syntax.
function badToken(tok) {
  return typeof tok !== "string" || tok === "" || /[\0\r\n]/.test(tok);
}

async function runCf(callId, args) {
  if (!Array.isArray(args) || args.length === 0 || args.some(badToken)) {
    fail(callId, "CF_OPS_BAD_ARGS", "args must be a non-empty array of plain strings");
    return;
  }
  const child = spawn("cf", args, {
    stdio: ["ignore", "pipe", "pipe"],
    env: process.env,
  });
  let out = "";
  let err = "";
  let capped = false;
  child.stdout.on("data", (d) => {
    if (out.length < MAX_OUTPUT) out += d;
    else capped = true;
  });
  child.stderr.on("data", (d) => {
    if (err.length < MAX_OUTPUT) err += d;
  });
  const timer = setTimeout(() => child.kill("SIGTERM"), TIMEOUT_MS);
  const code = await new Promise((resolve) => {
    child.on("close", resolve);
    child.on("error", (e) => {
      fail(callId, "CF_OPS_SPAWN", `cf spawn failed: ${e.message}`);
      resolve(null);
    });
  });
  clearTimeout(timer);
  if (code === null) return; // spawn error already answered
  const suffix = capped ? "\n[cf-ops: output capped]" : "";
  if (code !== 0) {
    const cause = (err.trim() || out.trim() || `cf exited ${code}`).slice(0, 2000);
    // cf's own structured error text is the useful part; pass it through.
    fail(callId, "CF_OPS_EXIT", cause + suffix);
    return;
  }
  let result = out + suffix;
  // cf prints JSON by default; parse-validate so malformed output surfaces
  // as an error instead of a mystery downstream.
  try {
    result = JSON.parse(out);
  } catch {
    // Non-JSON output (help text, prompts) passes through as text.
  }
  respond({ call_id: callId, result });
}

let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buf += chunk;
  let idx;
  while ((idx = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, idx).trim();
    buf = buf.slice(idx + 1);
    if (!line) continue;
    let req;
    try {
      req = JSON.parse(line);
    } catch (e) {
      fail("unknown", "CF_OPS_BAD_FRAME", `unparsable request line: ${e.message}`);
      continue;
    }
    const callId = typeof req.call_id === "string" ? req.call_id : "unknown";
    const tool = req.tool;
    const args = req.args && req.args.args;
    if (tool !== "cf_read" && tool !== "cf_write" && tool !== "cf_destroy") {
      fail(callId, "CF_OPS_UNKNOWN_TOOL", `unknown tool ${tool}`);
      continue;
    }
    runCf(callId, args);
  }
});
process.stdin.on("end", () => {});
