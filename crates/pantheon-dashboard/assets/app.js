/* Pantheon dashboard SPA — vanilla JS, no build step.
   Every view: loading / empty / error states. Every mutation: confirm.
   Design: VibePrompt metric row + sparklines + delta tables; Raycast via
   Refero "midnight command center" (#040506, hairlines, inset highlights,
   Inter/mono labels, neutral buttons, mono footer). */
"use strict";

/* ---------------- auth ---------------- */
const qs = new URLSearchParams(location.search);
let TOKEN = qs.get("token") || sessionStorage.getItem("pantheon_token") || "";
if (qs.get("token")) {
  sessionStorage.setItem("pantheon_token", TOKEN);
  history.replaceState(null, "", location.pathname + location.hash);
}

const $ = (sel, el) => (el || document).querySelector(sel);
const $$ = (sel, el) => Array.from((el || document).querySelectorAll(sel));

/* ---------------- api ---------------- */
async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: Object.assign(
      { "X-Pantheon-Token": TOKEN },
      body !== undefined ? { "Content-Type": "application/json" } : {}
    ),
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  if (res.status === 401) throw { status: 401, message: "unauthorized — bad or missing token" };
  const ct = res.headers.get("content-type") || "";
  if (ct.includes("application/json")) {
    const data = await res.json();
    if (!res.ok) throw { status: res.status, message: data.message || data.error || ("HTTP " + res.status) };
    return data;
  }
  if (!res.ok) throw { status: res.status, message: "HTTP " + res.status };
  return res;
}

function download(path, filename) {
  const a = document.createElement("a");
  a.href = path + (path.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(TOKEN);
  a.download = filename || "";
  document.body.appendChild(a);
  a.click();
  a.remove();
}

/* ---------------- formatting ---------------- */
function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
function fmtNum(n) {
  if (n == null) return "—";
  return Number(n).toLocaleString("en-US");
}
function fmtCost(n) {
  if (n == null) return "—";
  return "$" + Number(n).toFixed(2);
}
function fmtTime(ms) {
  if (!ms) return "—";
  const d = new Date(ms);
  return d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}
function relTime(ms) {
  if (!ms) return "—";
  const s = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (s < 60) return s + "s ago";
  if (s < 3600) return Math.floor(s / 60) + "m ago";
  if (s < 86400) return Math.floor(s / 3600) + "h ago";
  return Math.floor(s / 86400) + "d ago";
}
function statusPill(s) {
  const map = { completed: "ok", running: "running", awaiting_approval: "awaiting_approval", failed: "failed", canceled: "warn", paused: "paused" };
  const key = map[s] || (s === "true" || s === true ? "ok" : "");
  return '<span class="pill" data-s="' + esc(key) + '">' + esc(s) + "</span>";
}
/* VibePrompt sparkline: tiny SVG polyline, no library. */
function spark(values, w, h) {
  w = w || 96; h = h || 24;
  if (!values || values.length < 2) return '<span class="text-faint mono" style="font-size:11px">—</span>';
  const max = Math.max.apply(null, values.concat([1]));
  const min = Math.min.apply(null, values.concat([0]));
  const span = max - min || 1;
  const pts = values.map((v, i) => {
    const x = (i / (values.length - 1)) * (w - 2) + 1;
    const y = h - 2 - ((v - min) / span) * (h - 4);
    return x.toFixed(1) + "," + y.toFixed(1);
  });
  const area = "1," + (h - 1) + " " + pts.join(" ") + " " + (w - 1) + "," + (h - 1);
  return '<svg class="spark" width="' + w + '" height="' + h + '" aria-hidden="true">' +
    '<polygon class="area" points="' + area + '"/>' +
    '<polyline points="' + pts.join(" ") + '"/></svg>';
}

/* ---------------- view chrome ---------------- */
const view = $("#view");
function setView(html) { view.innerHTML = html; view.focus({ preventScroll: true }); }
function loading(msg) {
  return '<div class="state"><div class="spinner"></div><div class="big">Loading</div>' + esc(msg || "") + "</div>";
}
function emptyState(big, sub) {
  return '<div class="state"><div class="big">' + esc(big) + '</div>' + esc(sub || "") + "</div>";
}
function errorState(msg, retry) {
  return '<div class="state"><div class="big state-error">Couldn\u2019t load</div>' +
    esc(msg) + (retry ? ' <div style="margin-top:10px"><button class="btn small" data-retry>Retry</button></div>' : "") + "</div>";
}
function viewHead(title, sub, actions) {
  return '<div class="view-head"><h1 class="view-title">' + esc(title) + "</h1>" +
    (sub ? '<span class="view-sub">' + esc(sub) + "</span>" : "") +
    '<span class="spacer"></span>' + (actions || "") + "</div>";
}

/* ---------------- toast & modal ---------------- */
function toast(msg, kind) {
  const t = document.createElement("div");
  t.className = "toast";
  if (kind) t.dataset.k = kind;
  t.textContent = msg;
  $("#toast-root").appendChild(t);
  setTimeout(() => t.remove(), 4200);
}
/* Confirm modal. Returns a promise<boolean>. Every mutation goes through it. */
function confirmDialog({ title, body, confirmLabel, danger }) {
  return new Promise((resolve) => {
    const root = $("#modal-root");
    root.innerHTML =
      '<div class="modal-veil"><div class="modal" role="dialog" aria-modal="true" aria-label="' + esc(title) + '">' +
      "<h3>" + esc(title) + "</h3>" + (body || "") +
      '<div class="m-actions"><button class="btn" data-x>Cancel</button>' +
      '<button class="btn ' + (danger ? "danger" : "primary") + '" data-ok>' + esc(confirmLabel || "Confirm") + "</button></div></div></div>";
    const close = (v) => { root.innerHTML = ""; resolve(v); };
    $("[data-x]", root).onclick = () => close(false);
    $("[data-ok]", root).onclick = () => close(true);
    $(".modal-veil", root).addEventListener("mousedown", (e) => { if (e.target.classList.contains("modal-veil")) close(false); });
    $("[data-ok]", root).focus();
  });
}
/* Preview modal for name-only diffs: shows changed names, then asks to apply. */
async function previewThenApply({ title, intro, names, apply }) {
  const namesHtml = names.length
    ? '<div class="name-list">' + names.map((n) => "<div>" + esc(n) + "</div>").join("") + "</div>"
    : '<p class="view-sub">No changes.</p>';
  const ok = await confirmDialog({
    title,
    body: '<p class="m-sub">' + esc(intro) + " — names only:</p>" + namesHtml,
    confirmLabel: "Apply",
  });
  if (!ok) return false;
  await apply();
  return true;
}

/* ---------------- nav & routing ---------------- */
const VIEWS = [
  ["overview", "Overview"],
  ["runs", "Runs"],
  ["approvals", "Approvals", () => state.pendingApprovals],
  ["schedule", "Schedule"],
  ["stats", "Stats"],
  ["memory", "Memory"],
  ["config", "Config"],
  ["keys", "Keys"],
  ["logs", "Logs"],
  ["skills", "Skills"],
  ["mcp", "MCP"],
  ["system", "System"],
];
const state = { pendingApprovals: 0, route: "overview", param: null };
const renderers = {};

function buildNav() {
  $("#nav").innerHTML = VIEWS.map(([id, label, badge]) =>
    '<button data-view="' + id + '" aria-current="' + (state.route === id ? "page" : "false") + '">' +
    esc(label) + (badge ? '<span class="count" data-badge="' + id + '"></span>' : "") + "</button>"
  ).join("");
  $$("#nav button").forEach((b) => {
    b.onclick = () => { location.hash = "#/" + b.dataset.view; };
  });
}
function updateBadges() {
  const el = $('[data-badge="approvals"]');
  if (el) {
    el.textContent = state.pendingApprovals || "";
    el.classList.toggle("hot", state.pendingApprovals > 0);
    el.hidden = !state.pendingApprovals;
  }
}
function navigate() {
  const parts = (location.hash || "#/overview").replace(/^#\//, "").split("/");
  state.route = parts[0] || "overview";
  state.param = parts[1] ? decodeURIComponent(parts.slice(1).join("/")) : null;
  if (!renderers[state.route]) state.route = "overview";
  buildNav();
  renderers[state.route]();
}
window.addEventListener("hashchange", navigate);

/* ---------------- KPI row ---------------- */
async function refreshKpis() {
  const el = $("#kpis");
  try {
    const [ov, stats] = await Promise.all([
      api("GET", "/api/overview"),
      api("GET", "/api/stats?days=14").catch(() => null),
    ]);
    state.pendingApprovals = ov.approvals_pending || 0;
    updateBadges();
    const byDay = stats ? stats.by_day.map((d) => d.totals.total_tokens) : [];
    const cost = ov.last_24h.cost_usd || 0;
    el.innerHTML = [
      ["Runs", fmtNum(ov.runs.total), Object.entries(ov.runs.by_status || {}).map(([k, v]) => k + " " + v).join(" · ")],
      ["Approvals", fmtNum(ov.approvals_pending), "pending"],
      ["Cost 24h", fmtCost(cost), ""],
      ["Tokens 24h", fmtNum(ov.last_24h.tokens), spark(byDay)],
      ["Schedule", fmtNum(ov.schedule.total), (ov.schedule.active || 0) + " active"],
      ["Gateway", "", ""], // filled below
    ].map(([label, value, sub]) =>
      '<div class="kpi"><div class="k-label">' + label + '</div><div class="k-value">' + value + '</div>' +
      (sub ? '<div class="k-sub">' + sub + "</div>" : "") + "</div>"
    ).join("");
    // gateway cell
    try {
      const gw = await api("GET", "/api/gateway/status");
      const cells = $$(".kpi", el);
      const last = cells[cells.length - 1];
      last.querySelector(".k-value").innerHTML =
        '<span class="dot" data-state="' + (gw.running ? "running" : "warn") + '"></span>';
      last.querySelector(".k-value").style.fontSize = "14px";
      const sub = last.querySelector(".k-sub");
      if (sub) sub.textContent = (gw.installed || "not installed") + (gw.running ? " · running" : " · stopped");
      const gl = $("#gateway-label");
      if (gl) {
        gl.textContent = "gateway " + (gw.running ? "running" : "stopped");
        $("#gateway-dot .dot").dataset.state = gw.running ? "running" : "warn";
      }
    } catch (e) { /* gateway status is best-effort */ }
  } catch (e) {
    el.innerHTML = '<div class="kpi"><div class="k-label">Status</div><div class="k-value state-error" style="font-size:13px">offline</div></div>';
  }
}

setInterval(() => { $("#foot-clock").textContent = new Date().toLocaleTimeString(); }, 1000);

/* ---------------- overview ---------------- */
renderers.overview = async function () {
  setView(viewHead("Overview", "last 24 hours and recent activity") + loading("overview"));
  try {
    const [ov, runs, appr] = await Promise.all([
      api("GET", "/api/overview"),
      api("GET", "/api/runs?limit=8"),
      api("GET", "/api/approvals"),
    ]);
    const recent = runs.runs.map((r) =>
      "<tr class='rowlink' data-run='" + esc(r.id) + "' tabindex='0'>" +
      "<td class='mono'>" + esc(r.id.slice(0, 12)) + "</td>" +
      "<td>" + esc(r.title || "—") + "</td><td>" + statusPill(r.status) + "</td>" +
      "<td class='mono'>" + esc(r.model || "—") + "</td>" +
      "<td class='mono' style='text-align:right'>" + fmtNum(r.input_tokens + r.output_tokens) + "</td>" +
      "<td class='mono' style='text-align:right'>" + fmtCost(r.cost_usd) + "</td>" +
      "<td class='mono'>" + relTime(r.created_ms) + "</td></tr>"
    ).join("");
    const approvals = appr.approvals.map((a) =>
      "<tr><td class='mono'>" + esc(a.run_id.slice(0, 12)) + "</td>" +
      "<td class='mono'>" + esc(a.tool) + "</td>" +
      "<td class='mono' style='max-width:420px;overflow:hidden;text-overflow:ellipsis'>" + esc(a.args) + "</td>" +
      "<td><span class='btn-row'><button class='btn small' data-grant='" + esc(a.id) + "'>Grant</button>" +
      "<button class='btn small danger' data-deny='" + esc(a.id) + "'>Deny</button></span></td></tr>"
    ).join("");
    setView(
      viewHead("Overview", "last 24 hours and recent activity",
        '<button class="btn small" id="ov-refresh">Refresh</button>') +
      '<div class="grid-2">' +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Recent runs</span><span class="spacer"></span>' +
      '<a href="#/runs" class="mono" style="font-size:11px">all runs</a></div>' +
      (recent ? '<div class="panel-body flush"><table class="grid"><thead><tr><th>Run</th><th>Title</th><th>Status</th><th>Model</th><th style="text-align:right">Tokens</th><th style="text-align:right">Cost</th><th>Created</th></tr></thead><tbody>' + recent + "</tbody></table></div>"
        : emptyState("No runs yet", "Runs appear here once the agent completes work.")) +
      "</div>" +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Pending approvals</span><span class="spacer"></span>' +
      '<a href="#/approvals" class="mono" style="font-size:11px">review</a></div>' +
      (approvals ? '<div class="panel-body flush"><table class="grid"><thead><tr><th>Run</th><th>Tool</th><th>Arguments</th><th></th></tr></thead><tbody>' + approvals + "</tbody></table></div>"
        : emptyState("Nothing waiting", "Approval requests from running agents land here.")) +
      "</div></div>"
    );
    $("#ov-refresh").onclick = () => { renderers.overview(); refreshKpis(); };
    $$("[data-run]").forEach((tr) => {
      const go = () => { location.hash = "#/runs/" + encodeURIComponent(tr.dataset.run); };
      tr.onclick = go;
      tr.onkeydown = (e) => { if (e.key === "Enter") go(); };
    });
    wireApprovalButtons();
  } catch (e) {
    setView(viewHead("Overview", "") + errorState(e.message, true));
    $("[data-retry]").onclick = () => renderers.overview();
  }
};

/* ---------------- runs ---------------- */
renderers.runs = async function () {
  if (state.param) return renderRunDetail(state.param);
  setView(viewHead("Runs", "search, export, prune") +
    '<div class="toolbar"><input id="rq" class="input" placeholder="search id or title" style="width:220px" aria-label="search runs">' +
    '<select id="rstatus" class="select" aria-label="filter by status"><option value="">all statuses</option>' +
    ["running", "completed", "failed", "canceled", "awaiting_approval"].map((s) => "<option>" + s + "</option>").join("") +
    '</select><button class="btn" id="rgo">Search</button></div>' +
    '<div id="rlist">' + loading("runs") + "</div>");
  const load = async () => {
    $("#rlist").innerHTML = loading("runs");
    try {
      const q = $("#rq").value.trim();
      const st = $("#rstatus").value;
      let path = "/api/runs?limit=100";
      if (q) path += "&q=" + encodeURIComponent(q);
      if (st) path += "&status=" + encodeURIComponent(st);
      const data = await api("GET", path);
      if (!data.runs.length) {
        $("#rlist").innerHTML = emptyState("No runs match", q || st ? "Try a different search." : "Runs appear here once the agent does work.");
        return;
      }
      $("#rlist").innerHTML = '<div class="panel"><div class="panel-body flush"><table class="grid"><thead><tr>' +
        "<th>Run</th><th>Title</th><th>Status</th><th>Model</th><th>Turns</th><th>Tools</th>" +
        '<th style="text-align:right">Tokens</th><th style="text-align:right">Cost</th><th>Created</th></tr></thead><tbody>' +
        data.runs.map((r) =>
          "<tr class='rowlink' data-run='" + esc(r.id) + "' tabindex='0'>" +
          "<td class='mono'>" + esc(r.id.slice(0, 12)) + "</td><td>" + esc(r.title || "—") + "</td>" +
          "<td>" + statusPill(r.status) + "</td><td class='mono'>" + esc(r.model || "—") + "</td>" +
          "<td class='mono'>" + fmtNum(r.turns) + "</td><td class='mono'>" + fmtNum(r.tool_calls) + "</td>" +
          "<td class='mono' style='text-align:right'>" + fmtNum(r.input_tokens + r.output_tokens) + "</td>" +
          "<td class='mono' style='text-align:right'>" + fmtCost(r.cost_usd) + "</td>" +
          "<td class='mono'>" + relTime(r.created_ms) + "</td></tr>"
        ).join("") + "</tbody></table></div></div>";
      $$("#rlist [data-run]").forEach((tr) => {
        const go = () => { location.hash = "#/runs/" + encodeURIComponent(tr.dataset.run); };
        tr.onclick = go;
        tr.onkeydown = (e) => { if (e.key === "Enter") go(); };
      });
    } catch (e) { $("#rlist").innerHTML = errorState(e.message, true); $("[data-retry]").onclick = load; }
  };
  $("#rgo").onclick = load;
  $("#rq").onkeydown = (e) => { if (e.key === "Enter") load(); };
  $("#rstatus").onchange = load;
  load();
};

async function renderRunDetail(id) {
  setView(viewHead("Run", id) + loading("run detail"));
  try {
    const r = await api("GET", "/api/runs/" + encodeURIComponent(id));
    const transcript = (r.transcript || []).map((m) =>
      m.type === "reasoning"
        ? '<div class="msg" data-role="reason"><div class="m-role">reasoning</div><div class="m-body">' + esc(m.content) + "</div></div>"
        : '<div class="msg" data-role="' + esc(m.role) + '"><div class="m-role">' + esc(m.role) + '</div><div class="m-body">' + esc(m.content) + "</div></div>"
    ).join("");
    const timeline = (r.timeline || []).map((t) =>
      "<li><span class='t-ts'>" + fmtTime(t.ts_ms) + "</span><span class='t-kind'>" + esc(t.kind) + "</span>" +
      "<span class='t-body'>" + esc(typeof t.detail === "string" ? t.detail : JSON.stringify(t.detail)) + "</span></li>"
    ).join("");
    setView(
      viewHead("Run " + id.slice(0, 12), r.title || "",
        '<span class="btn-row"><button class="btn small" id="rd-json">JSON</button>' +
        '<button class="btn small" id="rd-md">Markdown</button>' +
        '<button class="btn small danger" id="rd-prune">Prune</button></span>') +
      '<div class="panel"><div class="panel-body"><dl class="kv">' +
      "<dt>Status</dt><dd>" + statusPill(r.status) + "</dd>" +
      "<dt>Model</dt><dd class='mono'>" + esc(r.model || "—") + (r.provider ? " <span class='view-sub'>" + esc(r.provider) + "</span>" : "") + "</dd>" +
      "<dt>Tokens</dt><dd class='mono'>" + fmtNum(r.input_tokens + r.output_tokens) + " (" + fmtNum(r.input_tokens) + " in / " + fmtNum(r.output_tokens) + " out)</dd>" +
      "<dt>Cost</dt><dd class='mono'>" + fmtCost(r.cost_usd) + "</dd>" +
      "<dt>Turns / tools</dt><dd class='mono'>" + fmtNum(r.turns) + " / " + fmtNum(r.tool_calls) + "</dd>" +
      "<dt>Created</dt><dd class='mono'>" + fmtTime(r.created_ms) + "</dd>" +
      (r.ended_ms ? "<dt>Ended</dt><dd class='mono'>" + fmtTime(r.ended_ms) + "</dd>" : "") +
      "</dl></div></div>" +
      '<div class="section-label">Transcript</div><div class="panel"><div class="panel-body">' +
      (transcript || emptyState("No transcript", "This run recorded no messages.")) + "</div></div>" +
      '<div class="section-label">Timeline</div><div class="panel"><div class="panel-body flush">' +
      (timeline ? '<ul class="tl">' + timeline + "</ul>" : emptyState("No events", "")) + "</div></div>"
    );
    $("#rd-json").onclick = () => download("/api/runs/" + encodeURIComponent(id) + "/export?format=json", "run-" + id + ".json");
    $("#rd-md").onclick = () => download("/api/runs/" + encodeURIComponent(id) + "/export?format=md", "run-" + id + ".md");
    $("#rd-prune").onclick = async () => {
      const ok = await confirmDialog({
        title: "Prune run " + id.slice(0, 12) + "?",
        body: '<p class="m-sub">Deletes this run and all its events from the ledger. This cannot be undone.</p>',
        confirmLabel: "Prune run", danger: true,
      });
      if (!ok) return;
      try {
        const res = await api("DELETE", "/api/runs/" + encodeURIComponent(id) + "?confirm=true");
        toast("Pruned " + res.events_deleted + " events", "ok");
        location.hash = "#/runs";
      } catch (e) { toast("Prune failed: " + e.message, "err"); }
    };
  } catch (e) {
    setView(viewHead("Run", id) + errorState(e.message, true));
    $("[data-retry]").onclick = () => renderRunDetail(id);
  }
}

/* ---------------- approvals ---------------- */
function wireApprovalButtons(root) {
  $$("[data-grant], [data-deny]", root).forEach((b) => {
    b.onclick = async () => {
      const id = b.dataset.grant || b.dataset.deny;
      const granted = !!b.dataset.grant;
      const ok = await confirmDialog({
        title: (granted ? "Grant" : "Deny") + " approval?",
        body: '<p class="m-sub mono">' + esc(id) + "</p>",
        confirmLabel: granted ? "Grant" : "Deny",
        danger: !granted,
      });
      if (!ok) return;
      try {
        await api("POST", "/api/approvals/" + encodeURIComponent(id) + (granted ? "/grant" : "/deny"));
        toast(granted ? "Granted" : "Denied", "ok");
        renderers[state.route]();
        refreshKpis();
      } catch (e) { toast("Decision failed: " + e.message, "err"); }
    };
  });
}

renderers.approvals = async function () {
  setView(viewHead("Approvals", "pending across all runs") + loading("approvals"));
  try {
    const data = await api("GET", "/api/approvals");
    state.pendingApprovals = data.approvals.length;
    updateBadges();
    if (!data.approvals.length) {
      setView(viewHead("Approvals", "pending across all runs") +
        emptyState("Nothing waiting", "Approval requests from running agents land here."));
      return;
    }
    setView(viewHead("Approvals", data.approvals.length + " pending") +
      '<div class="panel"><div class="panel-body flush"><table class="grid"><thead><tr>' +
      "<th>Run</th><th>Tool</th><th>Call</th><th>Arguments (redacted)</th><th></th></tr></thead><tbody>" +
      data.approvals.map((a) =>
        "<tr><td><a class='mono' href='#/runs/" + encodeURIComponent(a.run_id) + "'>" + esc(a.run_id.slice(0, 12)) + "</a>" +
        (a.run_title ? "<div class='view-sub'>" + esc(a.run_title) + "</div>" : "") + "</td>" +
        "<td class='mono'>" + esc(a.tool) + "</td><td class='mono'>" + esc(a.call_id) + "</td>" +
        "<td class='mono' style='max-width:480px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap' title='" + esc(a.args) + "'>" + esc(a.args) + "</td>" +
        "<td><span class='btn-row'><button class='btn small' data-grant='" + esc(a.id) + "'>Grant</button>" +
        "<button class='btn small danger' data-deny='" + esc(a.id) + "'>Deny</button></span></td></tr>"
      ).join("") + "</tbody></table></div></div>");
    wireApprovalButtons();
  } catch (e) {
    setView(viewHead("Approvals", "") + errorState(e.message, true));
    $("[data-retry]").onclick = () => renderers.approvals();
  }
};

/* ---------------- schedule ---------------- */
function kindLabel(k) {
  if (!k) return "—";
  switch (k.type) {
    case "cron": return '<code class="inline">' + esc(k.expr) + "</code>";
    case "every": return "every " + esc(fmtDur(k.every_ms));
    case "oneshot": return "once " + esc(fmtTime(k.at_ms));
    case "webhook": return "webhook " + esc(k.path);
    case "conditional": return "when " + esc(k.expr);
    default: return esc(k.type || "manual");
  }
}
function fmtDur(ms) {
  if (ms == null) return "—";
  const s = Math.round(ms / 1000);
  if (s < 60) return s + "s";
  if (s < 3600) return Math.round(s / 60) + "m";
  if (s < 86400) return Math.round(s / 3600) + "h";
  return Math.round(s / 86400) + "d";
}

renderers.schedule = async function () {
  setView(viewHead("Schedule", "jobs, templates, delivery") +
    '<div class="toolbar"><button class="btn primary" id="sj-new">New job</button><span class="spacer" style="flex:1"></span>' +
    '<button class="btn small" id="sj-refresh">Refresh</button></div><div id="sjobs">' + loading("jobs") + "</div>");
  $("#sj-new").onclick = showJobForm;
  $("#sj-refresh").onclick = () => renderers.schedule();
  const load = async () => {
    try {
      const data = await api("GET", "/api/schedule/jobs");
      if (!data.jobs.length) {
        $("#sjobs").innerHTML = emptyState("No scheduled jobs", "Create one to run prompts on a cadence.");
        return;
      }
      $("#sjobs").innerHTML = '<div class="panel"><div class="panel-body flush"><table class="grid"><thead><tr>' +
        "<th>Task</th><th>Schedule</th><th>Next fire</th><th>State</th><th>Model</th><th>Deliver</th><th></th></tr></thead><tbody>" +
        data.jobs.map((j) =>
          "<tr><td style='max-width:340px'>" + esc(j.task.slice(0, 120)) + (j.task.length > 120 ? "…" : "") +
          "<div class='mono view-sub'>" + esc(j.id) + "</div></td>" +
          "<td>" + kindLabel(j.kind) + "</td>" +
          "<td class='mono'>" + (j.paused ? "—" : relTime(j.next_fire_ms).replace(" ago", "")) + "</td>" +
          "<td>" + (j.paused ? '<span class="pill" data-s="paused">paused</span>' : '<span class="pill" data-s="ok">active</span>') + "</td>" +
          "<td class='mono'>" + esc(j.model || "—") + "</td><td class='mono'>" + esc(j.deliver || "—") + "</td>" +
          "<td><span class='btn-row'>" +
          '<button class="btn small" data-jpause="' + esc(j.id) + '">' + (j.paused ? "Resume" : "Pause") + "</button>" +
          '<button class="btn small" data-jtrigger="' + esc(j.id) + '">Run now</button>' +
          '<button class="btn small" data-jedit="' + esc(j.id) + '">Edit</button>' +
          '<button class="btn small danger" data-jdel="' + esc(j.id) + '">Delete</button>' +
          "</span></td></tr>"
        ).join("") + "</tbody></table></div></div>";
      $$("#sjobs [data-jpause]").forEach((b) => b.onclick = async () => {
        const pausing = b.textContent === "Pause";
        const ok = await confirmDialog({ title: (pausing ? "Pause" : "Resume") + " job?", body: '<p class="m-sub mono">' + esc(b.dataset.jpause) + "</p>", confirmLabel: pausing ? "Pause" : "Resume" });
        if (!ok) return;
        try { await api("PUT", "/api/schedule/jobs/" + encodeURIComponent(b.dataset.jpause), { paused: pausing }); toast(pausing ? "Paused" : "Resumed", "ok"); renderers.schedule(); }
        catch (e) { toast(e.message, "err"); }
      });
      $$("#sjobs [data-jtrigger]").forEach((b) => b.onclick = async () => {
        const ok = await confirmDialog({ title: "Run job now?", body: '<p class="m-sub mono">' + esc(b.dataset.jtrigger) + "</p>", confirmLabel: "Run now" });
        if (!ok) return;
        try { await api("POST", "/api/schedule/jobs/" + encodeURIComponent(b.dataset.jtrigger) + "/trigger"); toast("Triggered", "ok"); }
        catch (e) { toast(e.message, "err"); }
      });
      $$("#sjobs [data-jdel]").forEach((b) => b.onclick = async () => {
        const ok = await confirmDialog({ title: "Delete job?", body: '<p class="m-sub mono">' + esc(b.dataset.jdel) + "</p>", confirmLabel: "Delete", danger: true });
        if (!ok) return;
        try { await api("DELETE", "/api/schedule/jobs/" + encodeURIComponent(b.dataset.jdel)); toast("Deleted", "ok"); renderers.schedule(); }
        catch (e) { toast(e.message, "err"); }
      });
      $$("#sjobs [data-jedit]").forEach((b) => b.onclick = () => showJobForm(b.dataset.jedit));
    } catch (e) { $("#sjobs").innerHTML = errorState(e.message, true); $("[data-retry]").onclick = load; }
  };
  load();
};

async function showJobForm(editId) {
  let job = null, templates = [];
  try { templates = (await api("GET", "/api/schedule/templates")).templates || []; } catch (e) { /* templates optional */ }
  if (editId) {
    try { job = (await api("GET", "/api/schedule/jobs")).jobs.find((j) => j.id === editId); } catch (e) { toast(e.message, "err"); return; }
  }
  const root = $("#modal-root");
  root.innerHTML = '<div class="modal-veil"><div class="modal" role="dialog" aria-modal="true" aria-label="' + (job ? "Edit job" : "New job") + '">' +
    "<h3>" + (job ? "Edit job" : "New job") + "</h3>" +
    (job ? "" :
      '<div class="field"><label>Template</label><select id="jf-tpl" class="select"><option value="">— none (raw task) —</option>' +
      templates.map((t) => '<option value="' + esc(t.name) + '">' + esc(t.name) + " — " + esc(t.description || "") + "</option>").join("") +
      "</select></div><div id='jf-vars'></div>") +
    '<div class="field"><label>Task / prompt</label><textarea id="jf-task" class="input" rows="4">' + esc(job ? job.task : "") + "</textarea></div>" +
    '<div class="form-grid">' +
    '<div class="field"><label>Every (duration, e.g. 30m)</label><input id="jf-every" class="input" value="' + esc(job && job.kind.type === "every" ? fmtDur(job.kind.every_ms) : "") + '"></div>' +
    '<div class="field"><label>Cron expression</label><input id="jf-cron" class="input" value="' + esc(job && job.kind.type === "cron" ? job.kind.expr : "") + '" placeholder="*/30 * * * *"></div>' +
    '<div class="field"><label>Model</label><input id="jf-model" class="input" value="' + esc(job ? job.model || "" : "") + '"></div>' +
    '<div class="field"><label>Deliver to</label><input id="jf-deliver" class="input" value="' + esc(job ? job.deliver || "" : "") + '"></div>' +
    "</div>" +
    '<div class="m-actions"><button class="btn" data-x>Cancel</button><button class="btn primary" data-ok>' + (job ? "Save" : "Create") + "</button></div></div></div>";
  const close = () => { root.innerHTML = ""; };
  $("[data-x]", root).onclick = close;
  const tplSel = $("#jf-tpl", root);
  if (tplSel) tplSel.onchange = () => {
    const t = templates.find((x) => x.name === tplSel.value);
    $("#jf-vars", root).innerHTML = t
      ? '<div class="section-label">Template variables</div>' + t.vars.map((v) =>
        '<div class="field"><label>' + esc(v.name) + (v.default ? ' <span class="hint">(default: ' + esc(v.default) + ")</span>" : "") + "</label>" +
        '<input class="input" data-var="' + esc(v.name) + '" value="' + esc(v.default || "") + '"' + (v.description ? ' title="' + esc(v.description) + '"' : "") + "></div>"
      ).join("") : "";
  };
  $("[data-ok]", root).onclick = async () => {
    const body = {
      task: $("#jf-task", root).value,
      every: $("#jf-every", root).value.trim() || undefined,
      cron: $("#jf-cron", root).value.trim() || undefined,
      model: $("#jf-model", root).value.trim() || undefined,
      deliver: $("#jf-deliver", root).value.trim() || undefined,
    };
    if (tplSel && tplSel.value) {
      body.template = tplSel.value;
      body.vars = {};
      $$("[data-var]", root).forEach((i) => { if (i.value) body.vars[i.dataset.var] = i.value; });
    }
    Object.keys(body).forEach((k) => body[k] === undefined && delete body[k]);
    try {
      if (job) await api("PUT", "/api/schedule/jobs/" + encodeURIComponent(job.id), body);
      else await api("POST", "/api/schedule/jobs", body);
      toast(job ? "Saved" : "Created", "ok");
      close();
      renderers.schedule();
    } catch (e) { toast(e.message, "err"); }
  };
  $(".modal-veil", root).addEventListener("mousedown", (e) => { if (e.target.classList.contains("modal-veil")) close(); });
  $("#jf-task", root).focus();
}

/* ---------------- stats (VibePrompt delta tables) ---------------- */
renderers.stats = async function () {
  let days = 30;
  const render = async () => {
    setView(viewHead("Usage", "token and cost rollups from the ledger") +
      '<div class="toolbar"><select id="st-days" class="select" aria-label="window">' +
      [7, 14, 30, 90].map((d) => '<option value="' + d + '"' + (d === days ? " selected" : "") + ">" + d + " days</option>").join("") +
      "</select></div><div id='st-body'>" + loading("stats") + "</div>");
    $("#st-days").onchange = (e) => { days = +e.target.value; render(); };
    try {
      const s = await api("GET", "/api/stats?days=" + days);
      const t = s.totals;
      const dayVals = s.by_day.map((d) => d.totals.total_tokens);
      const deltaRow = (rows) => rows.map((r) => {
        const tt = r.totals;
        return "<tr><td class='mono'>" + esc(r.model || r.run_id || r.day) + "</td>" +
          (r.title ? "<td>" + esc(r.title) + "</td>" : "") +
          "<td class='mono' style='text-align:right'>" + fmtNum(tt.calls) + "</td>" +
          "<td class='mono' style='text-align:right'>" + fmtNum(tt.input_tokens) + "</td>" +
          "<td class='mono' style='text-align:right'>" + fmtNum(tt.output_tokens) + "</td>" +
          "<td class='mono' style='text-align:right'>" + fmtNum(tt.total_tokens) + "</td>" +
          "<td class='mono' style='text-align:right'>" + fmtCost(tt.cost_usd) + "</td></tr>";
      }).join("");
      $("#st-body").innerHTML =
        '<div class="panel"><div class="panel-head"><span class="panel-title">Totals · ' + days + 'd</span><span class="spacer"></span>' + spark(dayVals, 160, 28) + "</div>" +
        '<div class="panel-body"><dl class="kv">' +
        "<dt>Model calls</dt><dd class='mono'>" + fmtNum(t.calls) + "</dd>" +
        "<dt>Tokens in / out</dt><dd class='mono'>" + fmtNum(t.input_tokens) + " / " + fmtNum(t.output_tokens) + "</dd>" +
        "<dt>Total tokens</dt><dd class='mono'>" + fmtNum(t.total_tokens) + "</dd>" +
        "<dt>Cost</dt><dd class='mono'>" + fmtCost(t.cost_usd) + (t.priced_calls < t.calls ? " <span class='view-sub'>(" + t.priced_calls + " of " + t.calls + " priced)</span>" : "") + "</dd>" +
        "</dl></div></div>" +
        '<div class="section-label">By model</div><div class="panel"><div class="panel-body flush">' +
        (s.by_model.length ? '<table class="grid"><thead><tr><th>Model</th><th style="text-align:right">Calls</th><th style="text-align:right">In</th><th style="text-align:right">Out</th><th style="text-align:right">Total</th><th style="text-align:right">Cost</th></tr></thead><tbody>' + deltaRow(s.by_model) + "</tbody></table>"
          : emptyState("No usage in window", "")) + "</div></div>" +
        '<div class="section-label">By run</div><div class="panel"><div class="panel-body flush">' +
        (s.by_run.length ? '<table class="grid"><thead><tr><th>Run</th><th>Title</th><th style="text-align:right">Calls</th><th style="text-align:right">In</th><th style="text-align:right">Out</th><th style="text-align:right">Total</th><th style="text-align:right">Cost</th></tr></thead><tbody>' +
          s.by_run.slice(0, 25).map((r) =>
            "<tr><td class='mono'><a href='#/runs/" + encodeURIComponent(r.run_id) + "'>" + esc(r.run_id.slice(0, 12)) + "</a></td>" +
            "<td>" + esc(r.title || "—") + "</td>" +
            "<td class='mono' style='text-align:right'>" + fmtNum(r.totals.calls) + "</td>" +
            "<td class='mono' style='text-align:right'>" + fmtNum(r.totals.input_tokens) + "</td>" +
            "<td class='mono' style='text-align:right'>" + fmtNum(r.totals.output_tokens) + "</td>" +
            "<td class='mono' style='text-align:right'>" + fmtNum(r.totals.total_tokens) + "</td>" +
            "<td class='mono' style='text-align:right'>" + fmtCost(r.totals.cost_usd) + "</td></tr>"
          ).join("") + "</tbody></table>" : emptyState("No usage in window", "")) + "</div></div>" +
        '<div class="section-label">By day</div><div class="panel"><div class="panel-body flush">' +
        (s.by_day.length ? '<table class="grid"><thead><tr><th>Day (UTC)</th><th style="text-align:right">Calls</th><th style="text-align:right">Tokens</th><th style="text-align:right">Cost</th></tr></thead><tbody>' +
          s.by_day.map((r) => "<tr><td class='mono'>" + esc(r.day) + "</td><td class='mono' style='text-align:right'>" + fmtNum(r.totals.calls) + "</td>" +
            "<td class='mono' style='text-align:right'>" + fmtNum(r.totals.total_tokens) + "</td>" +
            "<td class='mono' style='text-align:right'>" + fmtCost(r.totals.cost_usd) + "</td></tr>").join("") + "</tbody></table>"
          : emptyState("No usage in window", "")) + "</div></div>";
    } catch (e) { $("#st-body").innerHTML = errorState(e.message, true); $("[data-retry]").onclick = render; }
  };
  render();
};

/* ---------------- memory ---------------- */
renderers.memory = async function () {
  setView(viewHead("Memory", "agent-layer records with provenance") +
    '<div class="toolbar"><input id="mq" class="input" placeholder="search keys and values" style="width:220px" aria-label="search memory">' +
    '<input id="mns" class="input mono" style="width:140px" value="nyx" aria-label="namespace">' +
    '<button class="btn" id="mgo">Search</button></div><div id="mbody">' + loading("memory") + "</div>");
  const load = async () => {
    $("#mbody").innerHTML = loading("memory");
    try {
      const q = $("#mq").value.trim(), ns = $("#mns").value.trim() || "nyx";
      let path = "/api/memory?limit=200&namespace=" + encodeURIComponent(ns);
      if (q) path += "&q=" + encodeURIComponent(q);
      const data = await api("GET", path);
      if (!data.records.length) {
        $("#mbody").innerHTML = emptyState("No records", q ? "Try a different search." : "Nothing stored in this namespace yet.");
        return;
      }
      $("#mbody").innerHTML = '<div class="panel"><div class="panel-head"><span class="panel-title">' +
        esc(data.records.length) + " records · " + esc(data.namespace) + "</span></div>" +
        '<div class="panel-body flush"><table class="grid"><thead><tr><th>Key</th><th>Value</th><th>Trust</th><th>Origin</th><th>Recorded</th></tr></thead><tbody>' +
        data.records.map((r) =>
          "<tr><td class='mono'>" + esc(r.key) + "</td>" +
          "<td style='max-width:420px;word-break:break-word'>" + esc(r.value.length > 280 ? r.value.slice(0, 280) + "…" : r.value) + "</td>" +
          "<td>" + (r.provenance && r.provenance.trust ? '<span class="pill">' + esc(r.provenance.trust) + "</span>" : "—") + "</td>" +
          "<td class='mono view-sub'>" + esc(r.provenance && r.provenance.origin ? r.provenance.origin : "—") + "</td>" +
          "<td class='mono'>" + relTime(r.recorded_at_ms) + "</td></tr>"
        ).join("") + "</tbody></table></div></div>";
    } catch (e) { $("#mbody").innerHTML = errorState(e.message, true); $("[data-retry]").onclick = load; }
  };
  $("#mgo").onclick = load;
  $("#mq").onkeydown = (e) => { if (e.key === "Enter") load(); };
  load();
};

/* ---------------- config ---------------- */
function configInput(f) {
  const val = f.value;
  const id = "cfg-" + f.path.replace(/[^a-zA-Z0-9]/g, "_");
  if (f.enum) {
    return '<select class="input" id="' + id + '" data-path="' + esc(f.path) + '">' +
      f.enum.map((o) => '<option value="' + esc(o) + '"' + (String(val) === o ? " selected" : "") + ">" + esc(o) + "</option>").join("") + "</select>";
  }
  if (f.type === "bool") {
    return '<select class="input" id="' + id + '" data-path="' + esc(f.path) + '">' +
      ["true", "false"].map((o) => '<option value="' + o + '"' + (String(val) === o ? " selected" : "") + ">" + o + "</option>").join("") + "</select>";
  }
  if (f.type === "secret_ref") {
    const src = val && val.source ? val.source : "env";
    const name = val && val.name ? val.name : "";
    return '<div class="form-grid"><div class="field"><label>source</label><input class="input mono" data-path="' + esc(f.path) + '.source" value="' + esc(src) + '"></div>' +
      '<div class="field"><label>env var name</label><input class="input mono" data-path="' + esc(f.path) + '.name" value="' + esc(name) + '"></div></div>' +
      '<div class="hint">Values are never stored here — only the env var <em>name</em>. Manage values under Keys.</div>';
  }
  if (f.type === "integer" || f.type === "float") {
    return '<input class="input mono" id="' + id + '" data-path="' + esc(f.path) + '" data-type="' + f.type + '" value="' + esc(val == null ? "" : val) + '" inputmode="numeric">';
  }
  if (f.type === "table" || f.type === "array") {
    return '<textarea class="input mono" id="' + id + '" data-path="' + esc(f.path) + '" data-json="1" rows="3">' + esc(JSON.stringify(val == null ? (f.type === "array" ? [] : {}) : val, null, 1)) + "</textarea>" +
      '<div class="hint">JSON value</div>';
  }
  return '<input class="input" id="' + id + '" data-path="' + esc(f.path) + '" value="' + esc(val == null ? "" : val) + '">';
}

renderers.config = async function () {
  setView(viewHead("Config", "config.toml — validated, atomic writes") +
    '<div class="toolbar"><button class="btn primary" id="cf-save">Review changes</button>' +
    '<button class="btn" id="cf-export">Export</button><button class="btn" id="cf-import">Import</button>' +
    '<span class="view-sub" id="cf-dirty"></span></div><div id="cf-body">' + loading("config schema") + "</div>");
  try {
    const schema = await api("GET", "/api/config/schema");
    const fields = schema.fields || [];
    if (!fields.length) {
      $("#cf-body").innerHTML = emptyState("No config fields", "config.toml is empty or unreadable.");
      return;
    }
    // Group by top-level section for the form.
    const groups = {};
    fields.forEach((f) => {
      const top = f.path.includes(".") ? f.path.split(".")[0] : "(top level)";
      (groups[top] = groups[top] || []).push(f);
    });
    // Keep the original value per path so only real edits are sent.
    const originals = {};
    fields.forEach((f) => { originals[f.path] = JSON.stringify(f.value); });
    $("#cf-body").innerHTML = Object.keys(groups).sort().map((g) =>
      '<div class="section-label">' + esc(g) + "</div><div class='panel'><div class='panel-body'>" +
      groups[g].map((f) =>
        '<div class="field"><label class="mono">' + esc(f.path) +
        ' <span class="hint">' + esc(f.type) + "</span></label>" + configInput(f) + "</div>"
      ).join("") + "</div></div>"
    ).join("");
    const collectChanges = () => {
      const changes = {};
      $$("#cf-body [data-path]").forEach((el) => {
        const path = el.dataset.path;
        let v;
        if (el.dataset.json) {
          try { v = JSON.parse(el.value); } catch (e) { throw "invalid JSON at " + path; }
        } else if (el.dataset.type === "integer") {
          if (el.value.trim() === "") return;
          v = parseInt(el.value, 10);
          if (!Number.isFinite(v)) throw "not an integer at " + path;
        } else if (el.dataset.type === "float") {
          if (el.value.trim() === "") return;
          v = parseFloat(el.value);
          if (!Number.isFinite(v)) throw "not a number at " + path;
        } else v = el.value;
        // secret_ref: merge .source/.name back into one object at the base path
        const m = path.match(/^(.*)\.(source|name)$/);
        if (m && originals[m[1]] !== undefined) {
          changes[m[1]] = changes[m[1]] || JSON.parse(originals[m[1]] || '{"source":"env","name":""}');
          changes[m[1]][m[2]] = v;
          return;
        }
        if (JSON.stringify(v) !== originals[path]) changes[path] = v;
      });
      // drop unchanged secret_refs
      Object.keys(changes).forEach((k) => { if (JSON.stringify(changes[k]) === originals[k]) delete changes[k]; });
      return changes;
    };
    const markDirty = () => {
      try {
        const n = Object.keys(collectChanges()).length;
        $("#cf-dirty").textContent = n ? n + " unsaved change" + (n > 1 ? "s" : "") : "";
      } catch (e) { $("#cf-dirty").textContent = ""; }
    };
    $("#cf-body").addEventListener("input", markDirty);
    $("#cf-save").onclick = async () => {
      let changes;
      try { changes = collectChanges(); } catch (e) { toast(e, "err"); return; }
      if (!Object.keys(changes).length) { toast("No changes to apply"); return; }
      try {
        const preview = await api("PUT", "/api/config", { changes, confirm: false });
        const applied = await previewThenApply({
          title: "Apply config changes?",
          intro: "The server validated these " + preview.changes.length + " field(s). Writes are atomic",
          names: preview.changes,
          apply: async () => {
            const res = await api("PUT", "/api/config", { changes, confirm: true });
            toast("Applied " + res.changed.length + " change(s)", "ok");
            renderers.config();
          },
        });
        if (!applied) toast("Discarded");
      } catch (e) { toast(e.message, "err"); }
    };
    $("#cf-export").onclick = () => download("/api/config/export", "config.toml");
    $("#cf-import").onclick = () => {
      const root = $("#modal-root");
      root.innerHTML = '<div class="modal-veil"><div class="modal" role="dialog" aria-modal="true" aria-label="Import config">' +
        "<h3>Import config</h3><p class='m-sub'>Paste TOML. It is validated field-by-field against the current schema before anything is written.</p>" +
        '<div class="field"><textarea id="ci-toml" class="input mono" rows="12" spellcheck="false"></textarea></div>' +
        '<div class="m-actions"><button class="btn" data-x>Cancel</button><button class="btn primary" data-ok>Validate</button></div></div></div>';
      const close = () => { root.innerHTML = ""; };
      $("[data-x]", root).onclick = close;
      $("[data-ok]", root).onclick = async () => {
        const toml = $("#ci-toml", root).value;
        try {
          const preview = await api("POST", "/api/config/import", { toml, confirm: false });
          close();
          await previewThenApply({
            title: "Apply imported config?",
            intro: "Import validated. These top-level paths differ",
            names: preview.changes,
            apply: async () => {
              const res = await api("POST", "/api/config/import", { toml, confirm: true });
              toast("Applied " + res.changed.length + " change(s)", "ok");
              renderers.config();
            },
          });
        } catch (e) { toast(e.message, "err"); }
      };
      $("#ci-toml", root).focus();
    };
  } catch (e) {
    $("#cf-body").innerHTML = errorState(e.message, true);
    $("[data-retry]").onclick = () => renderers.config();
  }
};

/* ---------------- keys (.env) ---------------- */
renderers.keys = async function () {
  setView(viewHead("Keys", ".env — values are write-only, never shown") +
    '<div class="toolbar"><button class="btn primary" id="ek-add">Add key</button>' +
    '<button class="btn small" id="ek-refresh">Refresh</button></div><div id="ek-body">' + loading("keys") + "</div>");
  $("#ek-refresh").onclick = () => renderers.keys();
  $("#ek-add").onclick = () => keyForm(null);
  const load = async () => {
    try {
      const data = await api("GET", "/api/env");
      if (!data.keys.length) {
        $("#ek-body").innerHTML = emptyState("No keys in .env", "Add one — values are write-only and never displayed.");
        return;
      }
      $("#ek-body").innerHTML = '<div class="panel"><div class="panel-body flush"><table class="grid"><thead><tr>' +
        "<th>Key</th><th>Value</th><th>Used by</th><th>Process env</th><th></th></tr></thead><tbody>" +
        data.keys.map((k) =>
          "<tr><td class='mono'>" + esc(k.key) + "</td>" +
          "<td class='mono'>" + esc(k.redacted) + "</td>" +
          "<td class='mono view-sub'>" + (k.used_by.length ? esc(k.used_by.join(", ")) : "—") + "</td>" +
          "<td>" + (k.shadowed_by_process_env ? '<span class="pill" data-s="warn">shadows file</span>' : '<span class="view-sub">—</span>') + "</td>" +
          "<td><span class='btn-row'><button class='btn small' data-kedit='" + esc(k.key) + "'>Rotate</button>" +
          "<button class='btn small danger' data-kdel='" + esc(k.key) + "'>Delete</button></span></td></tr>"
        ).join("") + "</tbody></table></div></div>";
      $$("#ek-body [data-kedit]").forEach((b) => b.onclick = () => keyForm(b.dataset.kedit));
      $$("#ek-body [data-kdel]").forEach((b) => b.onclick = async () => {
        const ok = await confirmDialog({ title: "Delete key " + b.dataset.kdel + "?", body: '<p class="m-sub">Removes it from .env. Services referencing it will fail until a new value is set.</p>', confirmLabel: "Delete", danger: true });
        if (!ok) return;
        try { await api("DELETE", "/api/env/" + encodeURIComponent(b.dataset.kdel)); toast("Deleted", "ok"); renderers.keys(); }
        catch (e) { toast(e.message, "err"); }
      });
    } catch (e) { $("#ek-body").innerHTML = errorState(e.message, true); $("[data-retry]").onclick = load; }
  };
  load();
};

function keyForm(existingKey) {
  const root = $("#modal-root");
  root.innerHTML = '<div class="modal-veil"><div class="modal" role="dialog" aria-modal="true" aria-label="' + (existingKey ? "Rotate key" : "Add key") + '">' +
    "<h3>" + (existingKey ? "Rotate key" : "Add key") + "</h3>" +
    '<p class="m-sub">Values are write-only: the server never returns them. The preview shows key names only.</p>' +
    '<div class="field"><label>Key name</label><input id="kf-key" class="input mono" value="' + esc(existingKey || "") + '"' + (existingKey ? " disabled" : "") + ' spellcheck="false"></div>' +
    '<div class="field"><label>Value</label><input id="kf-val" class="input mono" type="password" autocomplete="off" spellcheck="false"></div>' +
    '<div class="m-actions"><button class="btn" data-x>Cancel</button><button class="btn primary" data-ok>Review</button></div></div></div>';
  const close = () => { root.innerHTML = ""; };
  $("[data-x]", root).onclick = close;
  $("[data-ok]", root).onclick = async () => {
    const key = $("#kf-key", root).value.trim();
    const val = $("#kf-val", root).value;
    if (!key) { toast("Key name is required", "err"); return; }
    if (!val) { toast("Value is required", "err"); return; }
    try {
      const upserts = {};
      upserts[key] = val;
      const preview = await api("PUT", "/api/env", { upserts, confirm: false });
      close();
      const names = preview.added.concat(preview.changed.map((k) => k + " (changed)")).concat(preview.deleted.map((k) => k + " (deleted)"));
      await previewThenApply({
        title: existingKey ? "Rotate this key?" : "Add this key?",
        intro: "Write-only apply",
        names,
        apply: async () => {
          await api("PUT", "/api/env", { upserts, confirm: true });
          toast("Saved", "ok");
          renderers.keys();
        },
      });
    } catch (e) { toast(e.message, "err"); }
  };
  $("#kf-key", root).focus();
}

/* ---------------- logs ---------------- */
let logSource = null; // EventSource for follow mode
function stopFollow() { if (logSource) { logSource.close(); logSource = null; } }

renderers.logs = async function () {
  stopFollow();
  let source = "agent", level = "", grep = "", tail = 200, follow = false;
  setView(viewHead("Logs", "agent · errors · gateway") +
    '<div class="toolbar"><select id="lg-src" class="select" aria-label="log source">' +
    ["agent", "errors", "gateway"].map((s) => "<option>" + s + "</option>").join("") + "</select>" +
    '<select id="lg-level" class="select" aria-label="minimum level"><option value="">all levels</option>' +
    ["debug", "info", "warning", "error"].map((s) => "<option>" + s + "</option>").join("") + "</select>" +
    '<input id="lg-grep" class="input" placeholder="filter text" style="width:160px" aria-label="filter text">' +
    '<select id="lg-tail" class="select" aria-label="tail lines">' +
    [100, 200, 500, 1000].map((n) => '<option value="' + n + '"' + (n === 200 ? " selected" : "") + ">" + n + " lines</option>").join("") + "</select>" +
    '<button class="btn" id="lg-go">Load</button>' +
    '<button class="btn" id="lg-follow" aria-pressed="false">Follow</button></div>' +
    '<div class="panel"><div class="panel-body flush"><div id="lg-lines" class="log-lines">' + loading("logs") + "</div></div></div>");
  const paintLine = (l) => {
    const m = l.match(/^\S+ \S+ (\w+)/);
    const cls = m ? "lv-" + m[1] : "";
    return '<div class="' + cls + '">' + esc(l) + "</div>";
  };
  const load = async () => {
    stopFollow();
    $("#lg-follow").textContent = "Follow";
    $("#lg-follow").setAttribute("aria-pressed", "false");
    follow = false;
    $("#lg-lines").innerHTML = loading("logs");
    try {
      let path = "/api/logs?source=" + source + "&tail=" + tail;
      if (level) path += "&level=" + level;
      if (grep) path += "&grep=" + encodeURIComponent(grep);
      const data = await api("GET", path);
      $("#lg-lines").innerHTML = data.lines.length
        ? data.lines.map(paintLine).join("")
        : emptyState("No lines", data.note || "The log file has no matching lines yet.");
      $("#lg-lines").scrollTop = $("#lg-lines").scrollHeight;
    } catch (e) { $("#lg-lines").innerHTML = errorState(e.message, false); }
  };
  const startFollow = () => {
    let url = "/api/logs/stream?source=" + source + "&token=" + encodeURIComponent(TOKEN);
    if (level) url += "&level=" + level;
    if (grep) url += "&grep=" + encodeURIComponent(grep);
    $("#lg-lines").innerHTML = "";
    logSource = new EventSource(url);
    logSource.onmessage = (ev) => {
      try {
        const d = JSON.parse(ev.data);
        if (d.note) { $("#lg-lines").innerHTML += '<div class="view-sub">' + esc(d.note) + "</div>"; return; }
        if (d.line !== undefined) {
          $("#lg-lines").innerHTML += paintLine(d.line);
          $("#lg-lines").scrollTop = $("#lg-lines").scrollHeight;
        }
      } catch (e) { /* keep-alive or partial */ }
    };
    logSource.onerror = () => {
      if (logSource && logSource.readyState === EventSource.CLOSED) {
        $("#lg-lines").innerHTML += '<div class="state-error">stream closed</div>';
      }
    };
  };
  $("#lg-go").onclick = () => { source = $("#lg-src").value; level = $("#lg-level").value; grep = $("#lg-grep").value.trim(); tail = +$("#lg-tail").value; load(); };
  $("#lg-follow").onclick = () => {
    source = $("#lg-src").value; level = $("#lg-level").value; grep = $("#lg-grep").value.trim();
    follow = !follow;
    $("#lg-follow").textContent = follow ? "Stop" : "Follow";
    $("#lg-follow").setAttribute("aria-pressed", String(follow));
    if (follow) startFollow(); else { stopFollow(); load(); }
  };
  load();
};

/* ---------------- skills ---------------- */
renderers.skills = async function () {
  let scope = "";
  setView(viewHead("Skills", "discover, toggle, import") +
    '<div class="toolbar"><select id="sk-scope" class="select" aria-label="scope">' +
    '<option value="">all scopes</option><option value="pantheon">pantheon</option><option value="external">external</option></select>' +
    '<button class="btn primary" id="sk-import">Import</button>' +
    '<button class="btn small" id="sk-refresh">Refresh</button></div><div id="sk-body">' + loading("skills") + "</div>");
  $("#sk-refresh").onclick = () => renderers.skills();
  $("#sk-import").onclick = skillImportForm;
  const load = async () => {
    try {
      const data = await api("GET", "/api/skills" + (scope ? "?scope=" + scope : ""));
      if (!data.skills.length) {
        $("#sk-body").innerHTML = emptyState("No skills found", "Import one, or check the skill roots.");
        return;
      }
      $("#sk-body").innerHTML = '<div class="panel"><div class="panel-body flush"><table class="grid"><thead><tr>' +
        "<th>Skill</th><th>Description</th><th>Scope</th><th>State</th><th></th></tr></thead><tbody>" +
        data.skills.map((s) =>
          "<tr><td class='mono'>" + esc(s.name) + "<div class='view-sub'>" + esc(s.origin || "") + "</div></td>" +
          "<td>" + esc(s.description || "—") + "</td>" +
          "<td><span class='pill'>" + esc(s.scope) + "</span></td>" +
          "<td>" + (s.enabled ? '<span class="pill" data-s="enabled">enabled</span>' : '<span class="pill" data-s="disabled">disabled</span>') + "</td>" +
          "<td><span class='btn-row'><button class='btn small' data-sktoggle='" + esc(s.name) + "'>" + (s.enabled ? "Disable" : "Enable") + "</button>" +
          (s.scope === "pantheon" ? "<button class='btn small danger' data-skdel='" + esc(s.name) + "'>Delete</button>" : "") +
          "</span></td></tr>"
        ).join("") + "</tbody></table></div></div>" +
        '<p class="view-sub">Disabling removes the skill from the model\u2019s toolset on the next session start. External skills can be disabled, never deleted.</p>';
      $$("#sk-body [data-sktoggle]").forEach((b) => b.onclick = async () => {
        const disabling = b.textContent === "Disable";
        const ok = await confirmDialog({
          title: (disabling ? "Disable" : "Enable") + " skill " + b.dataset.sktoggle + "?",
          body: disabling ? '<p class="m-sub">The skill stays on disk but is no longer offered to the model.</p>' : '<p class="m-sub">The skill becomes available to the model again.</p>',
          confirmLabel: disabling ? "Disable" : "Enable",
        });
        if (!ok) return;
        try { await api("POST", "/api/skills/" + encodeURIComponent(b.dataset.sktoggle) + "/toggle"); toast(disabling ? "Disabled" : "Enabled", "ok"); load(); }
        catch (e) { toast(e.message, "err"); }
      });
      $$("#sk-body [data-skdel]").forEach((b) => b.onclick = async () => {
        const ok = await confirmDialog({ title: "Delete skill " + b.dataset.skdel + "?", body: '<p class="m-sub">Removes its directory from &lt;data_dir&gt;/skills. This cannot be undone.</p>', confirmLabel: "Delete", danger: true });
        if (!ok) return;
        try { await api("DELETE", "/api/skills/" + encodeURIComponent(b.dataset.skdel)); toast("Deleted", "ok"); load(); }
        catch (e) { toast(e.message, "err"); }
      });
    } catch (e) { $("#sk-body").innerHTML = errorState(e.message, true); $("[data-retry]").onclick = load; }
  };
  $("#sk-scope").onchange = (e) => { scope = e.target.value; load(); };
  load();
};

function skillImportForm() {
  const root = $("#modal-root");
  root.innerHTML = '<div class="modal-veil"><div class="modal" role="dialog" aria-modal="true" aria-label="Import skill">' +
    "<h3>Import skill</h3><p class='m-sub'>From a SKILL.md URL or a git repo. Files land under &lt;data_dir&gt;/skills.</p>" +
    '<div class="field"><label>SKILL.md URL</label><input id="si-url" class="input mono" placeholder="https://…" spellcheck="false"></div>' +
    '<div class="field"><label>or git repo</label><input id="si-repo" class="input mono" placeholder="https://github.com/…" spellcheck="false"></div>' +
    '<div class="field"><label>subpath (optional)</label><input id="si-sub" class="input mono" spellcheck="false"></div>' +
    '<div class="m-actions"><button class="btn" data-x>Cancel</button><button class="btn primary" data-ok>Import</button></div></div></div>';
  const close = () => { root.innerHTML = ""; };
  $("[data-x]", root).onclick = close;
  $("[data-ok]", root).onclick = async () => {
    const url = $("#si-url", root).value.trim(), repo = $("#si-repo", root).value.trim(), subpath = $("#si-sub", root).value.trim();
    if (!url && !repo) { toast("Give a URL or a repo", "err"); return; }
    const ok = await confirmDialog({ title: "Import skill?", body: '<p class="m-sub mono">' + esc(url || repo) + "</p>", confirmLabel: "Import" });
    if (!ok) return;
    try {
      const body = { confirm: true };
      if (url) body.url = url; else { body.repo = repo; if (subpath) body.subpath = subpath; }
      const res = await api("POST", "/api/skills/import", body);
      toast("Imported " + res.imported.length + " skill(s)", "ok");
      close();
      renderers.skills();
    } catch (e) { toast(e.message, "err"); }
  };
}

/* ---------------- MCP ---------------- */
renderers.mcp = async function () {
  setView(viewHead("MCP", "declared servers — ready means prepared, not attached") +
    '<div class="toolbar"><button class="btn primary" id="mcp-add">Add server</button>' +
    '<button class="btn" id="mcp-reload">Reload</button></div><div id="mcp-body">' + loading("mcp servers") + "</div>");
  $("#mcp-add").onclick = mcpAddForm;
  $("#mcp-reload").onclick = async () => {
    try { await api("POST", "/api/mcp/reload"); toast("Reloaded from disk", "ok"); } catch (e) { toast(e.message, "err"); }
    renderers.mcp();
  };
  const load = async () => {
    try {
      const data = await api("GET", "/api/mcp/servers");
      const groups = data.servers || [];
      const count = groups.reduce((n, g) => n + g.servers.length, 0);
      if (!count) {
        $("#mcp-body").innerHTML = emptyState("No MCP servers declared", "Add one, or migrate from another harness.") +
          (data.note ? '<p class="view-sub">' + esc(data.note) + "</p>" : "");
        return;
      }
      $("#mcp-body").innerHTML = groups.map((g) =>
        '<div class="section-label">' + esc(g.source) + "</div>" +
        '<div class="panel"><div class="panel-body flush"><table class="grid"><thead><tr>' +
        "<th>Server</th><th>Transport</th><th>Target</th><th>Readiness</th><th>State</th><th></th></tr></thead><tbody>" +
        g.servers.map((s) =>
          "<tr><td class='mono'>" + esc(s.name) +
          (s.needs_credentials ? "<div class='view-sub'>needs credentials</div>" : "") +
          (s.requires_env && s.requires_env.length ? "<div class='view-sub'>" + esc(s.requires_env.join(", ")) + "</div>" : "") + "</td>" +
          "<td class='mono'>" + esc(s.transport) + "</td>" +
          "<td class='mono view-sub' style='max-width:280px;overflow:hidden;text-overflow:ellipsis'>" + esc(s.command || s.url || "—") + "</td>" +
          "<td>" + (s.readiness ? '<span class="pill" data-s="warn">' + esc(s.readiness) + "</span>" : '<span class="pill" data-s="ok">ready</span>') + "</td>" +
          "<td>" + (s.enabled ? '<span class="pill" data-s="enabled">enabled</span>' : '<span class="pill" data-s="disabled">disabled</span>') + "</td>" +
          "<td><span class='btn-row'>" +
          '<button class="btn small" data-mtest="' + esc(s.name) + '">Test</button>' +
          '<button class="btn small" data-mtoggle="' + esc(s.name) + '" data-on="' + (s.enabled ? "1" : "0") + '">' + (s.enabled ? "Disable" : "Enable") + "</button>" +
          '<button class="btn small danger" data-mdel="' + esc(s.name) + '">Delete</button>' +
          "</span></td></tr>"
        ).join("") + "</tbody></table></div></div>"
      ).join("") + (data.note ? '<p class="view-sub">' + esc(data.note) + "</p>" : "");
      $$("#mcp-body [data-mtest]").forEach((b) => b.onclick = async () => {
        b.disabled = true;
        try {
          const res = await api("POST", "/api/mcp/servers/" + encodeURIComponent(b.dataset.mtest) + "/test");
          toast(res.name + ": " + (res.ok ? "OK" : "FAIL") + " — " + res.detail, res.ok ? "ok" : "err");
        } catch (e) { toast(e.message, "err"); }
        b.disabled = false;
      });
      $$("#mcp-body [data-mtoggle]").forEach((b) => b.onclick = async () => {
        const disabling = b.dataset.on === "1";
        const ok = await confirmDialog({ title: (disabling ? "Disable" : "Enable") + " " + b.dataset.mtoggle + "?", confirmLabel: disabling ? "Disable" : "Enable" });
        if (!ok) return;
        try { await api("POST", "/api/mcp/servers/" + encodeURIComponent(b.dataset.mtoggle) + (disabling ? "/disable" : "/enable")); toast(disabling ? "Disabled" : "Enabled", "ok"); load(); }
        catch (e) { toast(e.message, "err"); }
      });
      $$("#mcp-body [data-mdel]").forEach((b) => b.onclick = async () => {
        const ok = await confirmDialog({ title: "Delete server " + b.dataset.mdel + "?", body: '<p class="m-sub">Removes the declaration. This cannot be undone.</p>', confirmLabel: "Delete", danger: true });
        if (!ok) return;
        try { await api("DELETE", "/api/mcp/servers/" + encodeURIComponent(b.dataset.mdel)); toast("Deleted", "ok"); load(); }
        catch (e) { toast(e.message, "err"); }
      });
    } catch (e) { $("#mcp-body").innerHTML = errorState(e.message, true); $("[data-retry]").onclick = load; }
  };
  load();
};

function mcpAddForm() {
  const root = $("#modal-root");
  root.innerHTML = '<div class="modal-veil"><div class="modal" role="dialog" aria-modal="true" aria-label="Add MCP server">' +
    "<h3>Add MCP server</h3>" +
    '<div class="form-grid">' +
    '<div class="field"><label>Name</label><input id="ma-name" class="input mono" spellcheck="false"></div>' +
    '<div class="field"><label>Declaration file</label><input id="ma-source" class="input mono" value="pantheon" spellcheck="false"></div>' +
    '<div class="field"><label>Transport</label><select id="ma-transport" class="select"><option>stdio</option><option>http</option><option>sse</option></select></div>' +
    '<div class="field"><label>Command (stdio)</label><input id="ma-command" class="input mono" spellcheck="false"></div>' +
    "</div>" +
    '<div class="field"><label>URL (http/sse)</label><input id="ma-url" class="input mono" spellcheck="false"></div>' +
    '<div class="field"><label>Args (one per line, stdio)</label><textarea id="ma-args" class="input mono" rows="2" spellcheck="false"></textarea></div>' +
    '<div class="field"><label>Requires env (comma-separated names)</label><input id="ma-env" class="input mono" spellcheck="false"></div>' +
    '<div class="m-actions"><button class="btn" data-x>Cancel</button><button class="btn primary" data-ok>Add</button></div></div></div>';
  const close = () => { root.innerHTML = ""; };
  $("[data-x]", root).onclick = close;
  $("[data-ok]", root).onclick = async () => {
    const body = {
      confirm: true,
      source: $("#ma-source", root).value.trim() || "pantheon",
      name: $("#ma-name", root).value.trim(),
      transport: $("#ma-transport", root).value,
      command: $("#ma-command", root).value.trim() || undefined,
      url: $("#ma-url", root).value.trim() || undefined,
      args: $("#ma-args", root).value.split("\n").map((s) => s.trim()).filter(Boolean),
      requires_env: $("#ma-env", root).value.split(",").map((s) => s.trim()).filter(Boolean),
    };
    Object.keys(body).forEach((k) => body[k] === undefined && delete body[k]);
    if (!body.name) { toast("Name is required", "err"); return; }
    try {
      await api("POST", "/api/mcp/servers", body);
      toast("Added", "ok");
      close();
      renderers.mcp();
    } catch (e) { toast(e.message, "err"); }
  };
  $("#ma-name", root).focus();
}

/* ---------------- system: gateway, reflect, consolidate ---------------- */
renderers.system = async function () {
  setView(viewHead("System", "gateway service, reflection, consolidation") + loading("system status"));
  try {
    const [gw, refl, cons] = await Promise.all([
      api("GET", "/api/gateway/status"),
      api("GET", "/api/reflect/status"),
      api("GET", "/api/consolidate/status"),
    ]);
    setView(viewHead("System", "gateway service, reflection, consolidation") +
      '<div class="grid-2">' +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Gateway</span><span class="spacer"></span>' +
      '<button class="btn small danger" id="sy-gw-restart">Restart</button></div><div class="panel-body"><dl class="kv">' +
      "<dt>Detected</dt><dd class='mono'>" + esc(gw.detected) + "</dd>" +
      "<dt>Installed</dt><dd class='mono'>" + esc(gw.installed || "no") + "</dd>" +
      "<dt>Running</dt><dd>" + (gw.running ? '<span class="pill" data-s="ok">running</span>' : '<span class="pill" data-s="warn">stopped</span>') + "</dd>" +
      "</dl></div></div>" +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Reflection</span><span class="spacer"></span>' +
      '<span class="btn-row"><button class="btn small" id="sy-refl-dry">Dry run</button><button class="btn small primary" id="sy-refl-run">Run pass</button></span></div>' +
      '<div class="panel-body"><dl class="kv">' +
      "<dt>Loop</dt><dd>" + (refl.enabled ? '<span class="pill" data-s="ok">enabled</span>' : '<span class="pill" data-s="warn">manual only</span>') + "</dd>" +
      "<dt>Last pass</dt><dd class='mono' style='white-space:pre-wrap'>" + esc(refl.last_run || "no reflection passes yet") + "</dd>" +
      "</dl></div></div></div>" +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Consolidation</span><span class="spacer"></span>' +
      '<span class="btn-row"><button class="btn small" id="sy-cons-dry">Dry run</button><button class="btn small primary" id="sy-cons-run">Run pass</button></span></div>' +
      '<div class="panel-body"><dl class="kv">' +
      "<dt>Enabled</dt><dd>" + (cons.enabled ? '<span class="pill" data-s="ok">enabled</span>' : '<span class="pill" data-s="warn">disabled</span>') + "</dd>" +
      "<dt>Last run</dt><dd class='mono'>" + (cons.last_run_ms ? fmtTime(cons.last_run_ms) + " (" + relTime(cons.last_run_ms) + ")" : "never") + "</dd>" +
      "<dt>Last summary</dt><dd class='mono'>" + esc(cons.last_summary || "—") + "</dd>" +
      "<dt>Promoted keys</dt><dd class='mono'>" + fmtNum(cons.promoted_keys) + "</dd>" +
      "</dl></div></div>"
    );
    $("#sy-gw-restart").onclick = async () => {
      const ok = await confirmDialog({ title: "Restart gateway?", body: '<p class="m-sub">Runs <code class="inline">pantheon gateway restart</code> in the background. Chat surfaces may drop briefly.</p>', confirmLabel: "Restart", danger: true });
      if (!ok) return;
      try { await api("POST", "/api/gateway/restart", { confirm: true }); toast("Restart requested", "ok"); }
      catch (e) { toast(e.message, "err"); }
    };
    const runPass = (kind, dry) => async () => {
      const ok = await confirmDialog({
        title: (dry ? "Dry-run " : "Run ") + kind + " pass?",
        body: dry ? '<p class="m-sub">No writes; the report goes to the log.</p>' : '<p class="m-sub">Runs the real CLI pass in the background.</p>',
        confirmLabel: dry ? "Dry run" : "Run pass",
      });
      if (!ok) return;
      try { await api("POST", "/api/" + kind + "/run", { confirm: true, dry_run: dry }); toast("Pass started", "ok"); }
      catch (e) { toast(e.message, "err"); }
    };
    $("#sy-refl-run").onclick = runPass("reflect", false);
    $("#sy-refl-dry").onclick = runPass("reflect", true);
    $("#sy-cons-run").onclick = runPass("consolidate", false);
    $("#sy-cons-dry").onclick = runPass("consolidate", true);
  } catch (e) {
    setView(viewHead("System", "") + errorState(e.message, true));
    $("[data-retry]").onclick = () => renderers.system();
  }
};

/* ---------------- boot ---------------- */
(function boot() {
  if (!TOKEN) {
    $("#gate").hidden = false;
    $("#gate-form").addEventListener("submit", (e) => {
      e.preventDefault();
      TOKEN = $("#gate-token").value.trim();
      if (!TOKEN) return;
      sessionStorage.setItem("pantheon_token", TOKEN);
      $("#gate").hidden = true;
      start();
    });
    $("#gate-token").focus();
    return;
  }
  start();
  async function start() {
    $("#app").hidden = false;
    refreshKpis();
    setInterval(refreshKpis, 30000);
    navigate();
    $("#foot-clock").textContent = new Date().toLocaleTimeString();
  }
})();
