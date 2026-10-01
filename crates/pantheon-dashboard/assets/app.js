/* Pantheon dashboard SPA. Vanilla JS, no build step.
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

/* 401 recovery (D-6): the token is per-instance and rotates on every
   restart, so a stale token 401s every view. Instead of dead Retry
   buttons, any 401 raises this modal gate: paste the current token,
   Reconnect stores it and re-renders the current view. Guarded so
   concurrent 401s raise it only once. */
function showReauthGate() {
  if ($("#reauth-veil") || !$("#modal-root")) return;
  const root = $("#modal-root");
  root.innerHTML =
    '<div class="modal-veil" id="reauth-veil"><div class="gate-card" role="dialog" aria-modal="true" aria-label="Reconnect">' +
    '<div class="gate-brand"><span class="brand-mark" aria-hidden="true"></span>Pantheon</div>' +
    '<p class="gate-sub"><strong>Token expired.</strong> The dashboard token rotates on every restart. ' +
    "Paste the current token printed by <span class='mono'>pantheon dashboard</span> at startup.</p>" +
    '<form id="reauth-form">' +
    '<input id="reauth-token" class="input" type="password" autocomplete="off" spellcheck="false" placeholder="token" aria-label="dashboard token">' +
    '<button class="btn primary" type="submit" style="margin-top:10px">Reconnect</button></form>' +
    '<p class="gate-note">Stored in this tab only (sessionStorage), sent as X-Pantheon-Token.</p>' +
    "</div></div>";
  $("#reauth-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const t = $("#reauth-token").value.trim();
    if (!t) return;
    TOKEN = t;
    sessionStorage.setItem("pantheon_token", TOKEN);
    root.innerHTML = "";
    if (typeof refreshStatus === "function") refreshStatus();
    navigate();
  });
  $("#reauth-token").focus();
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
  if (res.status === 401) {
    showReauthGate();
    throw { status: 401, reauth: true, message: "unauthorized: bad or missing token — reconnect with the current token" };
  }
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
/* Minimal safe markdown subset for inspector message bodies (D-11):
   escape first, then restore a small set of constructs — no raw HTML
   ever passes through. Covers fenced/inline code, headings, bold,
   italic, markdown links, bare-URL autolinks, lists, and blockquotes. */
function renderMd(text) {
  const stash = [];
  const put = (h) => { stash.push(h); return "\uE000" + (stash.length - 1) + "\uE000"; };
  let src = esc(text);
  src = src.replace(/```[^\S\n]*\w*\n([\s\S]*?)```/g, (m, code) =>
    put('<pre class="md-code"><code>' + code.replace(/^\n+|\s+$/g, "") + "</code></pre>"));
  const inline = (s) => s
    .replace(/`([^`\n]+)`/g, (m, c) => put("<code class='inline'>" + c + "</code>"))
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[\s(])\*([^*\n]+)\*/g, "$1<em>$2</em>")
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>')
    .replace(/(^|[\s(])(https?:\/\/[^\s<]+)/g, (m, pre, url) => {
      const trail = (url.match(/[.,;:!?)]+$/) || [""])[0];
      const clean = url.slice(0, url.length - trail.length);
      return pre + '<a href="' + clean + '" target="_blank" rel="noopener">' + clean + "</a>" + trail;
    });
  const lines = src.split("\n");
  let html = "", inList = null;
  const closeList = () => { if (inList) { html += inList === "ul" ? "</ul>" : "</ol>"; inList = null; } };
  for (const line of lines) {
    const t = line.trim();
    let m;
    if (!t) { closeList(); continue; }
    if (/^\uE000\d+\uE000$/.test(t)) { closeList(); html += t; }
    else if ((m = /^(#{1,4})\s+(.*)$/.exec(t))) { closeList(); html += '<div class="md-h' + m[1].length + '">' + inline(m[2]) + "</div>"; }
    else if ((m = /^&gt;\s?(.*)$/.exec(t))) { closeList(); html += '<div class="md-quote">' + inline(m[1]) + "</div>"; }
    else if ((m = /^[-*]\s+(.*)$/.exec(t))) {
      if (inList !== "ul") { closeList(); html += "<ul class='md-list'>"; inList = "ul"; }
      html += "<li>" + inline(m[1]) + "</li>";
    }
    else if ((m = /^\d+[.)]\s+(.*)$/.exec(t))) {
      if (inList !== "ol") { closeList(); html += "<ol class='md-list'>"; inList = "ol"; }
      html += "<li>" + inline(m[1]) + "</li>";
    }
    else { closeList(); html += "<p>" + inline(t) + "</p>"; }
  }
  closeList();
  return html.replace(/\uE000(\d+)\uE000/g, (m, i) => stash[+i]);
}
/* Rich link card from GET /api/link-preview (D-11). */
function linkCardHtml(p) {
  return '<a class="link-card" href="' + esc(p.url) + '" target="_blank" rel="noopener">' +
    (p.image ? '<img class="link-card-img" src="' + esc(p.image) + '" alt="" loading="lazy">' : "") +
    '<span class="link-card-body"><span class="link-card-title">' + esc(p.title || p.url) + "</span>" +
    (p.description ? '<span class="link-card-desc">' + esc(p.description) + "</span>" : "") +
    '<span class="link-card-domain mono">' + esc(p.domain || p.site_name || "") + "</span></span></a>";
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

/* ---------------- theme ---------------- */
function initTheme() {
  const saved = localStorage.getItem("pantheon_theme");
  document.documentElement.dataset.theme = saved === "light" ? "light" : "dark";
}
function toggleTheme() {
  const next = document.documentElement.dataset.theme === "light" ? "dark" : "light";
  document.documentElement.dataset.theme = next;
  localStorage.setItem("pantheon_theme", next);
}

/* ---------------- icons (inline SVG, stroke) ---------------- */
function icon(name, size) {
  const s = size || 16;
  const paths = {
    home: '<path d="m3 10 9-7 9 7v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/><path d="M9 22V12h6v10"/>',
    runs: '<polygon points="6 3 20 12 6 21 6 3"/>',
    approvals: '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/><path d="m9 12 2 2 4-4"/>',
    schedule: '<rect x="3" y="4" width="18" height="18" rx="2"/><path d="M16 2v4M8 2v4M3 10h18"/>',
    stats: '<path d="M3 3v18h18"/><path d="M7 15v3M12 10v8M17 6v12"/>',
    memory: '<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/>',
    config: '<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3"/><path d="M1 14h6M9 8h6M17 16h6"/>',
    keys: '<circle cx="7.5" cy="15.5" r="4.5"/><path d="m11 12 10-10M15 8l3 3"/>',
    logs: '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6M9 13h6M9 17h6"/>',
    skills: '<path d="M12 3v3m0 12v3M3 12h3m12 0h3M5.6 5.6l2.1 2.1m8.6 8.6 2.1 2.1m0-12.8-2.1 2.1M7.7 16.3l-2.1 2.1"/>',
    mcp: '<path d="M9 2v6M15 2v6M6 8h12v4a6 6 0 0 1-12 0z"/><path d="M12 18v4"/>',
    plugins: '<rect x="7" y="7" width="10" height="10" rx="2"/><path d="M10 7V4M14 7V4M10 20v-3M14 20v-3M7 10H4M7 14H4M20 10h-3M20 14h-3"/>',
    system: '<rect x="2" y="3" width="20" height="7" rx="2"/><rect x="2" y="14" width="20" height="7" rx="2"/><path d="M6 6.5h.01M6 17.5h.01"/>',
    zap: '<polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/>',
    clock: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 3"/>',
    check: '<path d="M20 6 9 17l-5-5"/>',
    plus: '<path d="M12 5v14M5 12h14"/>',
    search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>',
    arrow: '<path d="M5 12h14m-6-6 6 6-6 6"/>',
    back: '<path d="M19 12H5m6 6-6-6 6-6"/>',
    wrench: '<path d="M14.7 6.3a4.5 4.5 0 0 0-6 6L3 18l3 3 5.7-5.7a4.5 4.5 0 0 0 6-6L14 13l-3-3z"/>',
    bell: '<path d="M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.7 21a2 2 0 0 1-3.4 0"/>',
    profiles: '<circle cx="12" cy="8" r="4"/><path d="M4 21c0-4 3.6-6.5 8-6.5s8 2.5 8 6.5"/>',
    users: '<circle cx="9" cy="8" r="3.5"/><path d="M2.5 20c0-3.4 2.9-5.5 6.5-5.5s6.5 2.1 6.5 5.5"/><path d="M16 4.6a3.5 3.5 0 0 1 0 6.8M17.6 14.7c2.3.7 4 2.5 4 5.3"/>',
    swarm: '<circle cx="12" cy="5" r="2.4"/><circle cx="5" cy="19" r="2.4"/><circle cx="19" cy="19" r="2.4"/><path d="M12 7.4 6 16.8M12 7.4l6 9.4M7.4 19h9.2"/>',
  };
  return '<svg width="' + s + '" height="' + s + '" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
    (paths[name] || paths.home) + "</svg>";
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
    body: '<p class="m-sub">' + esc(intro) + ": names only:</p>" + namesHtml,
    confirmLabel: "Apply",
  });
  if (!ok) return false;
  await apply();
  return true;
}

/* ---------------- nav & routing ---------------- */
const VIEWS = [
  ["overview", "Overview", "home"],
  ["runs", "Runs", "runs"],
  ["approvals", "Approvals", "approvals", () => state.pendingApprovals],
  ["schedule", "Schedule", "schedule"],
  ["stats", "Stats", "stats"],
  ["memory", "Memory", "memory"],
  ["config", "Config", "config"],
  ["keys", "Keys", "keys"],
  ["logs", "Logs", "logs"],
  ["skills", "Skills", "skills"],
  ["mcp", "MCP", "mcp"],
  ["plugins", "Plugins", "plugins"],
  ["profiles", "Profiles", "profiles"],
  ["swarm", "Swarm", "swarm"],
  ["experts", "Experts", "users"],
  ["system", "System", "system"],
];
const state = { pendingApprovals: 0, route: "overview", param: null };
const renderers = {};

function buildNav() {
  $("#nav").innerHTML = VIEWS.map(([id, label, ic, badge]) =>
    '<button data-view="' + id + '" aria-current="' + (state.route === id ? "page" : "false") + '" title="' + esc(label) + '">' +
    icon(ic, 16) + '<span class="nav-label">' + esc(label) + "</span>" +
    (badge ? '<span class="count" data-badge="' + id + '"></span>' : "") + "</button>"
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
  stopFollow();
  stopInspPoll();
  const parts = (location.hash || "#/overview").replace(/^#\//, "").split("/");
  state.route = parts[0] || "overview";
  state.param = parts[1] ? decodeURIComponent(parts.slice(1).join("/")) : null;
  if (!renderers[state.route]) state.route = "overview";
  buildNav();
  renderers[state.route]();
}
window.addEventListener("hashchange", navigate);

/* ---------------- status (badges + gateway line) ---------------- */
async function refreshStatus() {
  try {
    const ov = await api("GET", "/api/overview");
    state.pendingApprovals = ov.approvals_pending || 0;
    updateBadges();
    try {
      const gw = await api("GET", "/api/gateway/status");
      const gl = $("#gateway-label");
      if (gl) {
        gl.textContent = "gateway " + (gw.running ? "running" : "stopped");
        const dot = $("#gateway-dot .dot");
        if (dot) dot.dataset.state = gw.running ? "running" : "warn";
      }
    } catch (e) { /* gateway status is best-effort */ }
  } catch (e) { /* offline: badges stay as-is */ }
}

/* Pending run search from the topbar. Consumed by renderers.runs. */
let pendingSearch = "";

/* ---------------- overview ---------------- */
function greeting() {
  const h = new Date().getHours();
  if (h < 12) return "Good morning";
  if (h < 18) return "Good afternoon";
  return "Good evening";
}

renderers.overview = async function () {
  setView(
    '<div class="ov-greet"><h1>' + greeting() + "</h1></div>" +
    '<div class="ov-cards">' +
    '<a class="ov-card" href="#/schedule"><span class="ov-card-icon">' + icon("schedule", 18) + '</span>' +
    '<span class="ov-card-title">Schedule a job</span><span class="ov-card-sub">Run prompts on a cadence</span>' +
    '<span class="ov-card-arrow">' + icon("arrow", 14) + "</span></a>" +
    '<a class="ov-card" href="#/runs"><span class="ov-card-icon">' + icon("runs", 18) + '</span>' +
    '<span class="ov-card-title">Browse runs</span><span class="ov-card-sub">Search, export, and prune</span>' +
    '<span class="ov-card-arrow">' + icon("arrow", 14) + "</span></a>" +
    '<a class="ov-card" href="#/memory"><span class="ov-card-icon">' + icon("memory", 18) + '</span>' +
    '<span class="ov-card-title">Memory</span><span class="ov-card-sub">Agent records with provenance</span>' +
    '<span class="ov-card-arrow">' + icon("arrow", 14) + "</span></a>" +
    '<a class="ov-card" href="#/skills"><span class="ov-card-icon">' + icon("wrench", 18) + '</span>' +
    '<span class="ov-card-title">Skills &amp; tools</span><span class="ov-card-sub">Discover, toggle, import</span>' +
    '<span class="ov-card-arrow">' + icon("arrow", 14) + "</span></a>" +
    "</div>" +
    '<div class="ov-grid"><div class="ov-main">' +
    '<div class="card"><div class="card-head"><span class="card-title">Recent runs</span>' +
    '<a class="card-link" href="#/runs">all runs</a></div><div id="ov-runs">' + loading("runs") + "</div></div>" +
    '<div class="card"><div class="card-head"><span class="card-title">Pending approvals</span>' +
    '<a class="card-link" href="#/approvals">review</a></div><div id="ov-approvals">' + loading("approvals") + "</div></div>" +
    '</div><div class="ov-side">' +
    '<div class="card"><div class="card-head"><span class="card-title">System overview</span>' +
    '<a class="card-link" href="#/system">details</a></div><div class="card-body" id="ov-system">' + loading("system") + "</div></div>" +
    '<div class="card"><div class="card-head"><span class="card-title">Quick actions</span></div>' +
    '<div class="card-body"><div class="qa-grid">' +
    '<a class="qa" href="#/schedule"><span class="qa-title">' + icon("plus", 14) + "New job</span><span class='qa-sub'>Create a schedule</span></a>" +
    '<a class="qa" href="#/stats"><span class="qa-title">' + icon("stats", 14) + "Usage</span><span class='qa-sub'>Tokens and cost</span></a>" +
    '<a class="qa" href="#/logs"><span class="qa-title">' + icon("logs", 14) + "Logs</span><span class='qa-sub'>Tail and follow</span></a>" +
    '<a class="qa" href="#/mcp"><span class="qa-title">' + icon("mcp", 14) + "MCP</span><span class='qa-sub'>Declared servers</span></a>" +
    '<a class="qa" href="#/keys"><span class="qa-title">' + icon("keys", 14) + "Keys</span><span class='qa-sub'>Manage .env secrets</span></a>" +
    '<a class="qa" href="#/config"><span class="qa-title">' + icon("config", 14) + "Config</span><span class='qa-sub'>Edit config.toml</span></a>" +
    "</div></div></div>" +
    '<div class="card"><div class="tabs" role="tablist">' +
    '<button role="tab" aria-selected="true" data-tab="recent">Recent activity</button>' +
    '<button role="tab" aria-selected="false" data-tab="upcoming">Upcoming</button></div>' +
    '<div id="ov-activity">' + loading("activity") + "</div></div>" +
    "</div></div>"
  );

  // Tabs
  let activityData = { recent: "", upcoming: "" };
  const paintActivity = (which) => {
    $$('.tabs [data-tab]').forEach((b) => b.setAttribute("aria-selected", String(b.dataset.tab === which)));
    $("#ov-activity").innerHTML = activityData[which] ||
      emptyState(which === "recent" ? "No recent activity" : "Nothing scheduled", "");
  };
  $$('.tabs [data-tab]').forEach((b) => { b.onclick = () => paintActivity(b.dataset.tab); });

  try {
    const [ov, runs, appr] = await Promise.all([
      api("GET", "/api/overview"),
      api("GET", "/api/runs?limit=8"),
      api("GET", "/api/approvals"),
    ]);
    state.pendingApprovals = appr.approvals.length;
    updateBadges();

    // ---- recent runs table ----
    const recent = runs.runs.map((r) =>
      "<tr class='rowlink' data-run='" + esc(r.id) + "' tabindex='0'>" +
      "<td><span class='agent-id'><span class='agent-avatar'>" + esc((r.title || r.id).trim().charAt(0).toUpperCase() || "R") + "</span>" +
      "<span><span class='agent-name'>" + esc(r.title || ("Run " + r.id.slice(0, 8))) + "</span><br>" +
      "<span class='agent-sub'>" + esc(r.id.slice(0, 12)) + "</span></span></span></td>" +
      "<td>" + statusPill(r.status) + "</td>" +
      "<td class='mono'>" + esc(r.model || "—") + "</td>" +
      "<td class='mono' style='text-align:right'>" + fmtNum(r.input_tokens + r.output_tokens) + "</td>" +
      "<td class='mono' style='text-align:right'>" + fmtCost(r.cost_usd) + "</td>" +
      "<td class='mono'>" + relTime(r.created_ms) + "</td></tr>"
    ).join("");
    $("#ov-runs").innerHTML = recent
      ? '<div class="card-body flush"><table class="agent-table"><thead><tr><th>Run</th><th>Status</th><th>Model</th>' +
        '<th style="text-align:right">Tokens</th><th style="text-align:right">Cost</th><th>Age</th></tr></thead><tbody>' +
        recent + "</tbody></table></div>"
      : emptyState("No runs yet", "Runs appear here once the agent completes work.");
    $$("#ov-runs [data-run]").forEach((tr) => {
      const go = () => { location.hash = "#/runs/" + encodeURIComponent(tr.dataset.run); };
      tr.onclick = go;
      tr.onkeydown = (e) => { if (e.key === "Enter") go(); };
    });

    // ---- pending approvals ----
    const approvals = appr.approvals.map((a) =>
      "<tr><td><span class='agent-id'><span class='agent-avatar'>" + esc((a.tool || "?").charAt(0).toUpperCase()) + "</span>" +
      "<span><span class='agent-name'>" + esc(a.tool) + "</span><br>" +
      "<span class='agent-sub'>" + esc(a.run_id.slice(0, 12)) + "</span></span></span></td>" +
      "<td class='mono' style='max-width:260px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap' title='" + esc(a.args) + "'>" + esc(a.args) + "</td>" +
      "<td><span class='btn-row'><button class='btn small' data-grant='" + esc(a.id) + "'>Grant</button>" +
      "<button class='btn small danger' data-deny='" + esc(a.id) + "'>Deny</button></span></td></tr>"
    ).join("");
    $("#ov-approvals").innerHTML = approvals
      ? '<div class="card-body flush"><table class="agent-table"><thead><tr><th>Tool</th><th>Arguments</th><th></th></tr></thead><tbody>' +
        approvals + "</tbody></table></div>"
      : emptyState("Nothing waiting", "Approval requests from running agents land here.");
    wireApprovalButtons($("#ov-approvals"));

    // ---- system overview (real metrics only) ----
    const sysParts = [];
    try {
      const [stats, jobs, gw] = await Promise.all([
        api("GET", "/api/stats?days=14").catch(() => null),
        api("GET", "/api/schedule/jobs").catch(() => null),
        api("GET", "/api/gateway/status").catch(() => null),
      ]);
      const byDay = stats ? stats.by_day.map((d) => d.totals.total_tokens) : [];
      const schedTotal = (jobs && jobs.jobs.length) || 0;
      const schedActive = jobs ? jobs.jobs.filter((j) => !j.paused).length : 0;
      const schedPct = schedTotal ? Math.round((schedActive / schedTotal) * 100) : 0;
      sysParts.push(
        '<div class="metric"><div class="metric-top"><span class="metric-label">Runs</span>' +
        '<span class="metric-value">' + fmtNum(ov.runs.total) + '</span></div>' +
        '<div class="metric-value dim">' + Object.entries(ov.runs.by_status || {}).map(([k, v]) => esc(k) + " " + fmtNum(v)).join(" · ") + "</div></div>",
        '<div class="metric"><div class="metric-top"><span class="metric-label">Pending approvals</span>' +
        '<span class="metric-value">' + fmtNum(ov.approvals_pending) + '</span></div></div>',
        '<div class="metric"><div class="metric-top"><span class="metric-label">Cost · 24h</span>' +
        '<span class="metric-value">' + fmtCost(ov.last_24h.cost_usd || 0) + '</span></div></div>',
        '<div class="metric"><div class="metric-top"><span class="metric-label">Tokens · 24h</span>' +
        '<span class="metric-value">' + fmtNum(ov.last_24h.tokens) + '</span></div>' +
        (byDay.length > 1 ? '<div style="margin-top:6px">' + spark(byDay, 220, 30) + "</div>" : "") + "</div>",
        '<div class="metric"><div class="metric-top"><span class="metric-label">Scheduled jobs</span>' +
        '<span class="metric-value">' + schedActive + " / " + schedTotal + ' active</span></div>' +
        (schedTotal ? '<div class="bar"><i style="width:' + schedPct + '%"></i></div>' : "") + "</div>",
        '<div class="metric"><div class="metric-top"><span class="metric-label">Gateway</span>' +
        (gw ? (gw.running
          ? '<span class="pill" data-s="ok">running</span>'
          : '<span class="pill" data-s="warn">stopped</span>')
          : '<span class="metric-value dim">—</span>') + "</div></div>"
      );
      // ---- activity tabs ----
      const items = [];
      runs.runs.slice(0, 6).forEach((r) => {
        items.push({
          ts: r.created_ms,
          ic: "runs",
          cls: r.status === "failed" ? "amber" : "",
          title: (r.status === "completed" ? "Run completed" : r.status === "failed" ? "Run failed" : r.status === "running" ? "Run started" : "Run " + r.status),
          sub: r.title || r.id.slice(0, 12),
        });
      });
      appr.approvals.slice(0, 6).forEach((a) => {
        items.push({
          ts: a.requested_ms || Date.now(),
          ic: "approvals",
          cls: "amber",
          title: "Approval requested",
          sub: a.tool + " · " + a.run_id.slice(0, 12),
        });
      });
      items.sort((x, y) => (y.ts || 0) - (x.ts || 0));
      activityData.recent = items.length
        ? '<ul class="activity">' + items.slice(0, 8).map((it) =>
          '<li><span class="act-icon ' + it.cls + '">' + icon(it.ic, 14) + '</span>' +
          '<span class="act-body"><span class="act-title">' + esc(it.title) + "</span><br>" +
          '<span class="act-sub">' + esc(it.sub) + "</span></span>" +
          '<span class="act-time">' + relTime(it.ts) + "</span></li>"
        ).join("") + "</ul>"
        : "";
      const upcoming = jobs ? jobs.jobs.filter((j) => !j.paused && j.next_fire_ms).sort((a, b) => a.next_fire_ms - b.next_fire_ms).slice(0, 8) : [];
      activityData.upcoming = upcoming.length
        ? '<ul class="activity">' + upcoming.map((j) =>
          '<li><span class="act-icon">' + icon("schedule", 14) + '</span>' +
          '<span class="act-body"><span class="act-title">' + esc(j.task.slice(0, 80)) + (j.task.length > 80 ? "…" : "") + "</span><br>" +
          '<span class="act-sub">' + esc(kindLabel(j.kind)) + "</span></span>" +
          '<span class="act-time">' + esc(fmtNextFire(j.next_fire_ms)) + "</span></li>"
        ).join("") + "</ul>"
        : "";
      paintActivity("recent");
    } catch (e) {
      sysParts.push(emptyState("Couldn't load system status", e.message));
    }
    $("#ov-system").innerHTML = sysParts.join("");
  } catch (e) {
    setView(viewHead("Overview", "") + errorState(e.message, true));
    const rb = $("[data-retry]");
    if (rb) rb.onclick = () => renderers.overview();
  }
};

/* ---------------- runs ---------------- */
renderers.runs = async function () {
  if (state.param) return renderers.inspector();
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
        const go = () => { location.hash = "#/inspector/" + encodeURIComponent(tr.dataset.run); };
        tr.onclick = go;
        tr.onkeydown = (e) => { if (e.key === "Enter") go(); };
      });
    } catch (e) { $("#rlist").innerHTML = errorState(e.message, true); $("[data-retry]").onclick = load; }
  };
  $("#rgo").onclick = load;
  $("#rq").onkeydown = (e) => { if (e.key === "Enter") load(); };
  $("#rstatus").onchange = load;
  if (pendingSearch) { $("#rq").value = pendingSearch; pendingSearch = ""; }
  load();
};

/* ---------------- run / subagent inspector ---------------- */
/* Display metadata for the event kinds served by GET /api/runs/:id. */
const INSP_KINDS = {
  run_started:        { label: "Run started",        milestone: true,  tone: "" },
  run_completed:      { label: "Run completed",      milestone: true,  tone: "ok" },
  run_failed:         { label: "Run failed",         milestone: true,  tone: "err" },
  run_canceled:       { label: "Run canceled",       milestone: true,  tone: "warn" },
  turn_started:       { label: "Turn started",       milestone: false, tone: "" },
  turn_completed:     { label: "Turn completed",     milestone: false, tone: "" },
  turn_parked:        { label: "Turn parked",        milestone: false, tone: "warn" },
  model_requested:    { label: "Model requested",    milestone: false, tone: "" },
  model_completed:    { label: "Model responded",    milestone: false, tone: "" },
  model_fallback:      { label: "Model fallback",     milestone: false, tone: "warn" },
  model_attempt_failed: { label: "Model attempt failed", milestone: false, tone: "err" },
  model_exhausted:    { label: "Provider chain exhausted", milestone: false, tone: "err" },
  usage:              { label: "Usage recorded",     milestone: false, tone: "" },
  approval_requested: { label: "Approval requested", milestone: false, tone: "warn" },
  approval_granted:   { label: "Approval granted",   milestone: false, tone: "ok" },
  approval_denied:    { label: "Approval denied",    milestone: false, tone: "err" },
  agent_spawned:      { label: "Subagent spawned",   milestone: false, tone: "accent" },
  agent_message:      { label: "Subagent message",   milestone: false, tone: "accent" },
  agent_completed:    { label: "Subagent completed", milestone: false, tone: "ok" },
  titled:             { label: "Session retitled",   milestone: false, tone: "" },
  tool_requested:    { label: "Tool requested",     milestone: false, tone: "accent" },
  tool_started:      { label: "Tool started",       milestone: false, tone: "accent" },
  tool_output:       { label: "Tool output",        milestone: false, tone: "" },
  tool_completed:    { label: "Tool completed",     milestone: false, tone: "ok" },
  other:              { label: "Event",              milestone: false, tone: "" },
};
function inspKind(k) { return INSP_KINDS[k] || INSP_KINDS.other; }

/* Execution timeline: milestone checkpoints, turn group headers, and
   clickable event nodes. Turn grouping is visual only. */
function inspTimelineHtml(timeline) {
  let html = "", lastTurn = null;
  timeline.forEach((t, i) => {
    const k = inspKind(t.kind);
    if (t.kind === "turn_started" && typeof t.detail === "string") {
      const m = /turn\s+(\S+)/i.exec(t.detail);
      if (m && m[1] !== lastTurn) {
        lastTurn = m[1];
        html += '<li class="ev-group">Turn ' + esc(m[1]) + "</li>";
      }
    }
    html +=
      '<li class="ev' + (k.milestone ? " milestone" : "") + '" data-ev="' + i + '" tabindex="0" role="button" aria-label="' + esc(k.label) + '">' +
      '<span class="ev-dot" data-tone="' + esc(k.tone) + '"></span>' +
      '<div class="ev-main"><div class="ev-top"><span class="ev-kind">' + esc(k.label) + "</span>" +
      '<span class="ev-ts mono">' + fmtTime(t.ts_ms) + "</span></div>" +
      (t.detail ? '<div class="ev-detail">' + esc(String(t.detail)) + "</div>" : "") +
      "</div></li>";
  });
  return html;
}

/* Detail viewport for the selected timeline event. */
function inspDetailHtml(t) {
  if (!t) return emptyState("No event selected", "Click an event in the timeline to inspect it.");
  const k = inspKind(t.kind);
  const isApproval = t.kind === "approval_requested" || t.kind === "approval_granted" || t.kind === "approval_denied";
  return '<dl class="kv">' +
    "<dt>Event</dt><dd>" + esc(k.label) + "</dd>" +
    "<dt>Time</dt><dd class='mono'>" + fmtTime(t.ts_ms) + "</dd>" +
    "<dt>Sequence</dt><dd class='mono'>#" + t.seq + "</dd>" +
    "<dt>Detail</dt><dd>" + (t.detail ? esc(String(t.detail)) : '<span class="text-faint">—</span>') + "</dd>" +
    (isApproval ? "<dt></dt><dd><a href='#/approvals'>Open approvals</a></dd>" : "") +
    "</dl>";
}

renderers.inspector = async function () {
  const id = state.param || "";
  if (inspRunId !== id) { inspRunId = id; inspSelSeq = null; inspQDraft = ""; }
  setView(viewHead("Inspector", "") + loading("run detail"));
  try {
    const r = await api("GET", "/api/runs/" + encodeURIComponent(id));
    const timeline = r.timeline || [];
    const title = r.title || ("Run " + id.slice(0, 12));
    const TERMINAL = { completed: 1, failed: 1, canceled: 1 };
    const isLive = !TERMINAL[r.status];
    const transcript = (r.transcript || []).map((m, i) =>
      m.type === "reasoning"
        ? '<div class="msg" data-role="reason"><div class="m-role">reasoning</div><div class="m-body">' + esc(m.content) + "</div></div>"
        : '<div class="msg" data-role="' + esc(m.role) + '"><div class="m-role">' + esc(m.role) + '</div><div class="m-body">' + renderMd(m.content) + '</div><div class="link-cards" id="lp-' + i + '"></div></div>'
    ).join("");
    setView(
      '<div class="insp-head"><a class="btn small" href="#/runs">' + icon("back", 14) + " Runs</a>" +
      statusPill(r.status) +
      '<h1 class="view-title">' + esc(title) + '</h1><span class="mono text-faint">' + esc(id.slice(0, 12)) + "</span>" +
      '<span class="spacer"></span><span class="btn-row">' +
      (r.status === "running" || r.status === "paused" || r.status === "awaiting_approval"
        ? '<button class="btn small danger" id="rd-cancel">Cancel</button>'
        : (r.status === "completed" || r.status === "failed" || r.status === "canceled"
          ? '<button class="btn small" id="rd-retry">Retry</button>' : "")) +
      '<button class="btn small" id="rd-json">JSON</button>' +
      '<button class="btn small" id="rd-md">Markdown</button>' +
      '<button class="btn small danger" id="rd-prune">Prune</button></span></div>' +
      '<div class="panel insp-queue"><div class="panel-body">' +
      '<div class="field" style="margin:0"><label for="rd-qmsg">Queue / steer a follow-up</label>' +
      '<div style="display:flex;gap:8px"><input class="input" id="rd-qmsg" placeholder="Message the agent…" style="flex:1" aria-label="queue message">' +
      '<button class="btn primary small" id="rd-qsend">Send</button></div>' +
      '<div class="hint">Busy run: the message is queued and runs when the current turn settles. Idle run: it starts a new turn.</div></div>' +
      "</div></div>" +
      '<div class="insp-grid">' +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Execution timeline</span><span class="spacer"></span>' +
      (isLive ? '<span class="pill" data-s="running" id="insp-live">live</span><button class="btn small" id="insp-pause">Pause</button>' : "") +
      '<span class="view-sub">' + timeline.length + " events</span></div>" +
      '<div class="panel-body">' +
      '<div class="ev-note">Per-tool-call detail is recorded in the ledger: requested / started / output / completed events appear below with the tool name, redacted arguments, and durations.</div>' +
      (timeline.length
        ? '<ol class="ev-list">' + inspTimelineHtml(timeline) + "</ol>"
        : emptyState("No events recorded", "This run has no ledger events yet.")) +
      "</div></div>" +
      '<div class="insp-side">' +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Run summary</span></div>' +
      '<div class="panel-body"><dl class="kv">' +
      "<dt>Status</dt><dd>" + statusPill(r.status) + "</dd>" +
      "<dt>Model</dt><dd class='mono'>" + esc(r.model || "—") + (r.provider ? " <span class='view-sub'>" + esc(r.provider) + "</span>" : "") + "</dd>" +
      "<dt>Tokens</dt><dd class='mono'>" + fmtNum(r.input_tokens + r.output_tokens) + " (" + fmtNum(r.input_tokens) + " in / " + fmtNum(r.output_tokens) + " out)</dd>" +
      "<dt>Cost</dt><dd class='mono'>" + fmtCost(r.cost_usd) + "</dd>" +
      "<dt>Turns / tools</dt><dd class='mono'>" + fmtNum(r.turns) + " / " + fmtNum(r.tool_calls) + "</dd>" +
      "<dt>Created</dt><dd class='mono'>" + fmtTime(r.created_ms) + "</dd>" +
      (r.ended_ms ? "<dt>Ended</dt><dd class='mono'>" + fmtTime(r.ended_ms) + "</dd>" : "") +
      "</dl></div></div>" +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Event detail</span></div>' +
      '<div class="panel-body" id="insp-detail">' + inspDetailHtml(null) + "</div></div>" +
      "</div>" +
      "</div>" +
      '<div class="section-label">Transcript</div><div class="panel"><div class="panel-body">' +
      (transcript || emptyState("No transcript", "This run recorded no messages.")) + "</div></div>"
    );
    const items = $$(".ev-list .ev");
    const select = (i) => {
      items.forEach((el) => el.classList.toggle("sel", +el.dataset.ev === i));
      $("#insp-detail").innerHTML = inspDetailHtml(timeline[i]);
      inspSelSeq = timeline[i] ? timeline[i].seq : null;
    };
    items.forEach((el) => {
      el.onclick = () => select(+el.dataset.ev);
      el.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); select(+el.dataset.ev); } };
    });
    if (timeline.length) {
      let def = timeline.length - 1;
      if (inspSelSeq != null) {
        // Keep the user's selection across live re-renders (matched by seq).
        const kept = timeline.findIndex((t) => t.seq === inspSelSeq);
        if (kept >= 0) def = kept;
      } else {
        for (let i = timeline.length - 1; i >= 0; i--) {
          if (inspKind(timeline[i].kind).milestone && timeline[i].kind !== "run_started") { def = i; break; }
        }
      }
      select(def);
    }
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
    // Link cards (D-11): bare URLs in assistant messages get og: previews
    // from GET /api/link-preview, rendered under the message. Capped so a
    // link-heavy transcript cannot fan out unboundedly; failures are silent.
    const lpByUrl = {};
    (r.transcript || []).forEach((m, i) => {
      if (m.type !== "message" || m.role !== "assistant") return;
      const urls = String(m.content || "").match(/https?:\/\/[^\s<>"')\]]+/g) || [];
      [...new Set(urls)].slice(0, 4).forEach((u) => {
        (lpByUrl[u] = lpByUrl[u] || []).push(i);
      });
    });
    Object.keys(lpByUrl).slice(0, 12).forEach((u) => {
      api("GET", "/api/link-preview?url=" + encodeURIComponent(u)).then((p) => {
        if (!p || !p.title) return;
        lpByUrl[u].forEach((i) => {
          const host = $("#lp-" + i);
          if (host) host.innerHTML += linkCardHtml(p);
        });
      }).catch(() => {});
    });
    // Run controls (D-7): cancel, retry, and the queue/steer box.
    const cxlBtn = $("#rd-cancel");
    if (cxlBtn) cxlBtn.onclick = async () => {
      const ok = await confirmDialog({
        title: "Cancel this run?",
        body: '<p class="m-sub">Asks the in-flight turn to wind down cooperatively — it stops at its next checkpoint.</p>',
        confirmLabel: "Cancel run", danger: true,
      });
      if (!ok) return;
      try {
        await api("POST", "/api/runs/" + encodeURIComponent(id) + "/cancel");
        toast("Cancel requested", "ok");
        renderers.inspector();
      } catch (e) { toast(e.message, "err"); }
    };
    const retBtn = $("#rd-retry");
    if (retBtn) retBtn.onclick = async () => {
      try {
        await api("POST", "/api/runs/" + encodeURIComponent(id) + "/retry");
        toast("Retrying the last turn", "ok");
        renderers.inspector();
      } catch (e) { toast(e.message, "err"); }
    };
    const qInput = $("#rd-qmsg");
    const qSend = async () => {
      const q = qInput.value.trim();
      if (!q) return;
      const body = { message: q };
      if (r.status === "running" || r.status === "paused") body.queue = true;
      try {
        const res = await api("POST", "/api/runs/" + encodeURIComponent(id) + "/message", body);
        qInput.value = "";
        inspQDraft = "";
        toast(res.steered ? "Steered — queued for the next turn" : res.queued ? "Message queued" : "Sent", "ok");
        renderers.inspector();
      } catch (e) { toast(e.message, "err"); }
    };
    if (qInput) {
      qInput.value = inspQDraft;
      qInput.addEventListener("input", () => { inspQDraft = qInput.value; });
      qInput.addEventListener("keydown", (e) => { if (e.key === "Enter") qSend(); });
      const qBtn = $("#rd-qsend");
      if (qBtn) qBtn.onclick = qSend;
    }
    // Live poll (D-16): while the run is non-terminal and not user-paused,
    // re-fetch every 3s. A re-render only happens when the status or the
    // last event seq changed, so the view doesn't flicker; the selected
    // event (by seq) and the queue draft survive re-renders.
    const pauseBtn = $("#insp-pause");
    if (pauseBtn) pauseBtn.onclick = () => {
      inspPaused = !inspPaused;
      pauseBtn.textContent = inspPaused ? "Resume" : "Pause";
      const pill = $("#insp-live");
      if (pill) { pill.textContent = inspPaused ? "paused" : "live"; pill.dataset.s = inspPaused ? "paused" : "running"; }
    };
    stopInspPoll();
    if (isLive) {
      inspPoll = setInterval(async () => {
        // The inspector is reachable as #/runs/<id> and #/inspector/<id>;
        // polling must survive both, and stop on any other navigation.
        if (inspPaused || (location.hash !== "#/runs/" + id && location.hash !== "#/inspector/" + id)) return;
        try {
          const cur = await api("GET", "/api/runs/" + encodeURIComponent(id));
          const tl = cur.timeline || [];
          const seq = tl.length ? tl[tl.length - 1].seq : null;
          const curSeq = timeline.length ? timeline[timeline.length - 1].seq : null;
          if (cur.status !== r.status || seq !== curSeq) renderers.inspector();
        } catch (e) { /* 401 raises the re-auth gate; anything else retries next tick */ }
      }, 3000);
    }
  } catch (e) {
    setView(viewHead("Inspector", "") + errorState(e.message, true));
    $("[data-retry]").onclick = () => renderers.inspector();
  }
};


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
        refreshStatus();
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
/* Next-fire countdown, future-aware (D-10): fires in the future render
   "in Xm", overdue jobs read "due now" instead of a negative duration. */
function fmtNextFire(ms) {
  if (ms == null) return "—";
  if (ms - Date.now() <= 0) return "due now";
  return "in " + fmtDur(ms - Date.now());
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
          "<td class='mono'>" + (j.paused ? "—" : fmtNextFire(j.next_fire_ms)) + "</td>" +
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
      templates.map((t) => '<option value="' + esc(t.name) + '">' + esc(t.name) + ": " + esc(t.description || "") + "</option>").join("") +
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
    return '<select class="input" id="' + id + '" data-path="' + esc(f.path) + '" data-type="bool">' +
      ["true", "false"].map((o) => '<option value="' + o + '"' + (String(val) === o ? " selected" : "") + ">" + o + "</option>").join("") + "</select>";
  }
  if (f.type === "datetime") {
    return '<input class="input mono" id="' + id + '" data-path="' + esc(f.path) + '" data-type="datetime" value="' + esc(val == null ? "" : val) + '" placeholder="ISO-8601, e.g. 2026-10-01T12:00:00Z">';
  }
  if (f.type === "secret_ref") {
    const src = val && val.source ? val.source : "env";
    const name = val && val.name ? val.name : "";
    return '<div class="form-grid"><div class="field"><label>source</label><input class="input mono" data-path="' + esc(f.path) + '.source" value="' + esc(src) + '"></div>' +
      '<div class="field"><label>env var name</label><input class="input mono" data-path="' + esc(f.path) + '.name" value="' + esc(name) + '"></div></div>' +
      '<div class="hint">Values are never stored here, only the env var <em>name</em>. Manage values under Keys.</div>';
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
  const rrBanner = (state.pendingRestart && state.pendingRestart.length)
    ? '<div class="notice warn" role="status">Restart required for <strong>' + esc(state.pendingRestart.join(", ")) +
      "</strong> to take effect — these sections are read once at startup." +
      '<span class="spacer"></span><button class="btn small" id="cf-rr-x">Dismiss</button></div>'
    : "";
  setView(viewHead("Config", "config.toml: validated, atomic writes") + rrBanner +
    '<div class="toolbar"><button class="btn primary" id="cf-save">Review changes</button>' +
    '<button class="btn" id="cf-export">Export</button><button class="btn" id="cf-import">Import</button>' +
    '<span class="view-sub" id="cf-dirty"></span></div><div id="cf-body">' + loading("config schema") + "</div>");
  const rrX = $("#cf-rr-x");
  if (rrX) rrX.onclick = () => { state.pendingRestart = null; rrX.closest(".notice").remove(); };
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
        } else if (el.dataset.type === "bool") {
          v = el.value === "true";
        } else if (el.dataset.type === "datetime") {
          if (el.value.trim() === "") return;
          v = el.value.trim();
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
            const rr = res.restart_required || [];
            if (rr.length) state.pendingRestart = rr;
            toast(
              "Applied " + res.changed.length + " change(s)" +
              (rr.length ? " — restart required for " + rr.join(", ") + " to take effect" : ""),
              rr.length ? "warn" : "ok"
            );
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
              const rr = res.restart_required || [];
              if (rr.length) state.pendingRestart = rr;
              toast(
                "Applied " + res.changed.length + " change(s)" +
                (rr.length ? " — restart required for " + rr.join(", ") + " to take effect" : ""),
                rr.length ? "warn" : "ok"
              );
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
  setView(viewHead("Keys", ".env: values are write-only, never shown") +
    '<div class="toolbar"><button class="btn primary" id="ek-add">Add key</button>' +
    '<button class="btn small" id="ek-refresh">Refresh</button></div><div id="ek-body">' + loading("keys") + "</div>");
  $("#ek-refresh").onclick = () => renderers.keys();
  $("#ek-add").onclick = () => keyForm(null);
  const load = async () => {
    try {
      const data = await api("GET", "/api/env");
      if (!data.keys.length) {
        $("#ek-body").innerHTML = emptyState("No keys in .env", "Add one. Values are write-only and never displayed.");
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

/* Inspector live-poll (D-16): refreshed on a 3s tick while the inspected
   run is in a non-terminal state. Cleared on every navigate(), exactly
   like follow mode above, so no timer outlives its view. */
let inspPoll = null;
let inspPaused = false;
let inspRunId = null;   // run the inspector is currently showing
let inspSelSeq = null;  // selected timeline event seq (preserved across live re-renders)
let inspQDraft = "";    // queue-box draft (preserved across live re-renders)
function stopInspPoll() { if (inspPoll) { clearInterval(inspPoll); inspPoll = null; } }

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
  setView(viewHead("MCP", "declared servers. Ready means prepared, not attached") +
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
          toast(res.name + " (" + (res.ok ? "OK" : "FAIL") + "): " + res.detail, res.ok ? "ok" : "err");
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

/* ---------------- plugins ---------------- */
/* Plugin imports, quarantine review, and approvals. Kept as one block;
   separate from the profile/swarm sections below. */
function pluginErrMsg(e) {
  if (e && e.message && typeof e.message === "object") {
    const inner = e.message.error || e.message;
    return inner.message || inner.code || ("HTTP " + e.status);
  }
  return (e && e.message) || "request failed";
}
function pluginScanPill(p) {
  if (!p.quarantined) return '<span class="pill">—</span>';
  const v = p.scan_verdict || "unknown";
  const s = v === "clean" ? "ok" : v === "suspicious" ? "warn" : "err";
  const n = p.scan_report && p.scan_report.findings ? p.scan_report.findings.length : 0;
  return '<span class="pill" data-s="' + s + '">' + esc(v) + "</span>" +
    (n ? ' <button class="btn small" data-scan="' + esc(p.name) + '">findings (' + n + ")</button>" : "");
}
function pluginStateCell(p) {
  if (p.quarantined) return '<span class="pill" data-s="warn">quarantined</span>';
  const bits = [];
  bits.push(p.enabled ? '<span class="pill" data-s="enabled">enabled</span>' : '<span class="pill" data-s="disabled">disabled</span>');
  if (!p.approved) bits.push('<span class="pill" data-s="warn">unapproved</span>');
  return bits.join(" ");
}
function pluginActions(p) {
  const n = esc(p.name), k = esc(p.kind);
  if (p.quarantined) {
    const v = p.scan_verdict;
    if (v === "malicious") {
      return "<span class='btn-row'><button class='btn small' disabled title='Blocked by the static scan'>Approve</button></span>";
    }
    return "<span class='btn-row'><button class='btn small primary' data-papprove='" + n + "' data-kind='" + k + "' data-verdict='" + esc(v || "") + "'>Approve</button></span>";
  }
  if (p.bundled) {
    return "<span class='btn-row'><button class='btn small' data-ptoggle='" + n + "' data-kind='" + k + "' data-on='" + (p.enabled ? "1" : "0") + "'>" + (p.enabled ? "Disable" : "Enable") + "</button></span>";
  }
  if (p.approved) {
    return "<span class='btn-row'><button class='btn small danger' data-pdisable='" + n + "' data-kind='" + k + "'>Disable</button></span>";
  }
  return "<span class='btn-row'><button class='btn small primary' data-papprove='" + n + "' data-kind='" + k + "'>Approve</button></span>";
}
renderers.plugins = async function () {
  setView(viewHead("Plugins", "tool and hook plugins. Imports are quarantined and scanned until approved") +
    '<div class="toolbar"><button class="btn primary" id="pl-import">Import</button>' +
    '<input id="pl-cq" class="input mono" placeholder="search ClawHub…" spellcheck="false" style="max-width:220px">' +
    '<button class="btn small" id="pl-csearch">Search</button>' +
    '<button class="btn small" id="pl-refresh">Refresh</button></div>' +
    '<div id="pl-reg"></div><div id="pl-body">' + loading("plugins") + "</div>");
  $("#pl-refresh").onclick = () => renderers.plugins();
  $("#pl-import").onclick = pluginImportForm;
  $("#pl-csearch").onclick = pluginRegistrySearch;
  $("#pl-cq").addEventListener("keydown", (e) => { if (e.key === "Enter") pluginRegistrySearch(); });
  const load = async () => {
    try {
      const data = await api("GET", "/api/plugins");
      const plugins = data.plugins || [];
      if (!plugins.length) {
        $("#pl-body").innerHTML = emptyState("No plugins installed", "Import one from a GitHub repo.");
        return;
      }
      const rows = plugins.map((p) =>
        "<tr><td class='mono'>" + esc(p.name) +
        "<div class='view-sub'>" + esc(p.kind) + " · v" + esc(p.version || "?") + "</div></td>" +
        "<td>" + esc(p.description || "—") +
        (p.privilege_notes ? "<div class='view-sub'>" + esc(p.privilege_notes) + "</div>" : "") + "</td>" +
        "<td>" + (p.bundled ? '<span class="pill">bundled</span>' : '<span class="pill">third-party</span>') +
        "<div class='view-sub mono'>" + esc(p.location || "") + "</div></td>" +
        "<td>" + pluginStateCell(p) + "</td>" +
        "<td>" + pluginScanPill(p) + "</td>" +
        "<td>" + pluginActions(p) + "</td></tr>"
      ).join("");
      $("#pl-body").innerHTML =
        '<div class="panel"><div class="panel-body flush"><table class="grid"><thead><tr>' +
        "<th>Plugin</th><th>Description</th><th>Source</th><th>State</th><th>Scan</th><th></th></tr></thead><tbody>" +
        rows + "</tbody></table></div></div>" +
        '<p class="view-sub">Quarantined imports never load until approved. A malicious scan verdict blocks approval; a suspicious one needs explicit risk acknowledgement.</p>';
      wirePluginButtons(load, plugins);
    } catch (e) { $("#pl-body").innerHTML = errorState(pluginErrMsg(e), true); $("[data-retry]").onclick = load; }
  };
  load();
};
function wirePluginButtons(reload, plugins) {
  const byName = {};
  plugins.forEach((p) => { byName[p.name] = p; });
  $$("#pl-body [data-scan]").forEach((b) => b.onclick = () => pluginScanModal(byName[b.dataset.scan]));
  $$("#pl-body [data-papprove]").forEach((b) => b.onclick = async () => {
    const p = byName[b.dataset.papprove];
    if (!p) return;
    if (p.quarantined && p.scan_verdict === "suspicious") {
      // Suspicious: show the findings and require explicit risk ack.
      const findings = (p.scan_report && p.scan_report.findings) || [];
      const list = findings.slice(0, 8).map((f) =>
        "<div class='mono' style='margin:4px 0'>[" + esc(f.severity) + "] " + esc(f.file) +
        (f.line ? ":" + f.line : "") + " — " + esc(f.rule) + "</div>"
      ).join("") || "<p class='m-sub'>No findings recorded.</p>";
      const ok = await confirmDialog({
        title: "Approve " + p.name + " despite the scan?",
        body: "<p class='m-sub'>The static scan flagged this plugin as suspicious:</p>" + list +
          (findings.length > 8 ? "<p class='m-sub'>…and " + (findings.length - 8) + " more. This is heuristic static analysis, not a sandbox.</p>" : "") +
          "<p class='m-sub'>Approving promotes it out of quarantine and lets it load.</p>",
        confirmLabel: "Approve anyway",
        danger: true,
      });
      if (!ok) return;
      try {
        await api("POST", "/api/plugins/" + encodeURIComponent(p.kind) + "/" + encodeURIComponent(p.name) + "/approve", { acknowledge_risk: true });
        toast("Approved " + p.name, "ok");
        reload();
      } catch (e) { toast(pluginErrMsg(e), "err"); }
      return;
    }
    const ok = await confirmDialog({
      title: "Approve " + p.name + "?",
      body: p.quarantined
        ? "<p class='m-sub'>Scan verdict: clean. Approving promotes it out of quarantine and lets it load.</p>"
        : (p.privilege_notes ? "<p class='m-sub'>" + esc(p.privilege_notes) + "</p>" : "<p class='m-sub'>Records operator approval so the plugin can load.</p>"),
      confirmLabel: "Approve",
    });
    if (!ok) return;
    try {
      await api("POST", "/api/plugins/" + encodeURIComponent(p.kind) + "/" + encodeURIComponent(p.name) + "/approve", {});
      toast("Approved " + p.name, "ok");
      reload();
    } catch (e) { toast(pluginErrMsg(e), "err"); }
  });
  $$("#pl-body [data-pdisable]").forEach((b) => b.onclick = async () => {
    const ok = await confirmDialog({
      title: "Disable " + b.dataset.pdisable + "?",
      body: '<p class="m-sub">Revokes approval; the plugin stays installed but will not load.</p>',
      confirmLabel: "Disable",
      danger: true,
    });
    if (!ok) return;
    try {
      await api("POST", "/api/plugins/" + encodeURIComponent(b.dataset.kind) + "/" + encodeURIComponent(b.dataset.pdisable) + "/disable", {});
      toast("Disabled", "ok");
      reload();
    } catch (e) { toast(pluginErrMsg(e), "err"); }
  });
  $$("#pl-body [data-ptoggle]").forEach((b) => b.onclick = async () => {
    const disabling = b.dataset.on === "1";
    const path = "/api/plugins/" + encodeURIComponent(b.dataset.kind) + "/" + encodeURIComponent(b.dataset.ptoggle) + (disabling ? "/disable" : "/approve");
    try {
      await api("POST", path, {});
      toast(disabling ? "Disabled" : "Enabled", "ok");
      reload();
    } catch (e) { toast(pluginErrMsg(e), "err"); }
  });
}
function pluginScanModal(p) {
  if (!p || !p.scan_report) return;
  const findings = p.scan_report.findings || [];
  const root = $("#modal-root");
  const rows = findings.length ? findings.map((f) =>
    "<tr><td><span class='pill' data-s='" + (f.severity === "critical" ? "err" : f.severity === "high" ? "err" : f.severity === "medium" ? "warn" : "ok") + "'>" + esc(f.severity) + "</span></td>" +
    "<td class='mono'>" + esc(f.file) + (f.line ? ":" + f.line : "") + "</td>" +
    "<td class='mono'>" + esc(f.rule) + "</td>" +
    "<td>" + esc(f.description) + "</td></tr>"
  ).join("") : "<tr><td colspan='4'>No findings.</td></tr>";
  root.innerHTML = '<div class="modal-veil"><div class="modal" role="dialog" aria-modal="true" aria-label="Scan report" style="max-width:720px">' +
    "<h3>Scan report: " + esc(p.name) + "</h3>" +
    "<p class='m-sub'>Verdict: <b>" + esc(p.scan_verdict || "unknown") + "</b>. Heuristic static analysis — not a sandbox, not a guarantee.</p>" +
    '<div class="panel"><div class="panel-body flush"><table class="grid"><thead><tr><th>Severity</th><th>File</th><th>Rule</th><th>Description</th></tr></thead><tbody>' +
    rows + "</tbody></table></div></div>" +
    '<div class="m-actions"><button class="btn primary" data-x>Close</button></div></div></div>';
  $("[data-x]", root).onclick = () => { root.innerHTML = ""; };
  $(".modal-veil", root).addEventListener("mousedown", (e) => { if (e.target.classList.contains("modal-veil")) root.innerHTML = ""; });
}
function pluginImportForm() {
  const root = $("#modal-root");
  root.innerHTML = '<div class="modal-veil"><div class="modal" role="dialog" aria-modal="true" aria-label="Import plugin">' +
    "<h3>Import plugin</h3><p class='m-sub'>From a GitHub repo URL or a <span class='mono'>clawhub:&lt;slug&gt;</span> reference. The bundle is downloaded, scanned, and quarantined — nothing loads until you approve it.</p>" +
    '<div class="field"><label>GitHub repo URL or clawhub:slug</label><input id="pi-url" class="input mono" placeholder="https://github.com/owner/repo or clawhub:slug" spellcheck="false"></div>' +
    '<div class="field"><label>ref (optional: branch, tag, commit)</label><input id="pi-ref" class="input mono" placeholder="main" spellcheck="false"></div>' +
    '<div id="pi-err" class="view-sub" style="color:var(--danger)"></div>' +
    '<div class="m-actions"><button class="btn" data-x>Cancel</button><button class="btn primary" data-ok>Import</button></div></div></div>';
  const close = () => { root.innerHTML = ""; };
  $("[data-x]", root).onclick = close;
  $("[data-ok]", root).onclick = async () => {
    const url = $("#pi-url", root).value.trim(), ref = $("#pi-ref", root).value.trim();
    if (!url) { toast("Give a URL or a clawhub:slug", "err"); return; }
    $("#pi-err", root).textContent = "";
    $("[data-ok]", root).disabled = true;
    try {
      const body = { url };
      if (ref) body.ref = ref;
      const res = await api("POST", "/api/plugins/import", body);
      toast("Imported " + res.name + " — quarantined, scan: " + (res.scan_report && res.scan_report.verdict), "ok");
      close();
      renderers.plugins();
    } catch (e) {
      // Structured 422s carry machine-readable extras on the thrown error object.
      const inner = e.message && typeof e.message === "object" ? e.message : {};
      if (inner.code === "NOT_A_PLUGIN" && inner.skill) {
        $("#pi-err", root).textContent = "That ClawHub entry is a skill (" + (inner.skill.name || inner.skill.slug) + "), not a plugin. " + (inner.hint || "Import it from the Skills page instead.");
      } else if (inner.code === "UNSUPPORTED_LAYOUT") {
        $("#pi-err", root).textContent = (inner.message || "Unsupported layout.") + (inner.found && inner.found.length ? " Found: " + inner.found.join(", ") : "");
      } else {
        $("#pi-err", root).textContent = pluginErrMsg(e);
      }
      $("[data-ok]", root).disabled = false;
    }
  };
  $("#pi-url", root).focus();
}
async function pluginRegistrySearch() {
  const q = $("#pl-cq").value.trim();
  if (!q) return;
  $("#pl-reg").innerHTML = '<p class="view-sub">Searching ClawHub…</p>';
  try {
    const data = await api("GET", "/api/plugins/registry/search?q=" + encodeURIComponent(q) + "&source=clawhub");
    const results = data.results || [];
    if (!results.length) {
      $("#pl-reg").innerHTML = '<p class="view-sub">No ClawHub results for "' + esc(q) + '".</p>';
      return;
    }
    $("#pl-reg").innerHTML = '<div class="section-label">ClawHub results</div>' +
      '<div class="panel"><div class="panel-body flush"><table class="grid"><thead><tr><th>Entry</th><th>Description</th><th>Version</th><th></th></tr></thead><tbody>' +
      results.map((r) =>
        "<tr><td class='mono'>" + esc(r.name || r.slug) + "<div class='view-sub'>" + esc(r.slug) + " · " + esc(r.kind) + "</div></td>" +
        "<td>" + esc(r.description || "—") + "</td>" +
        "<td class='mono'>" + esc(r.version || "—") + "</td>" +
        "<td><span class='view-sub'>ClawHub lists skills — import from the " +
        "<a href='#/skills'>Skills</a> page, not here.</span></td></tr>"
      ).join("") + "</tbody></table></div></div>";
  } catch (e) { $("#pl-reg").innerHTML = '<p class="view-sub">Search failed: ' + esc(pluginErrMsg(e)) + "</p>"; }
}

/* ---------------- system: gateway, reflect, consolidate ---------------- */
renderers.system = async function () {
  setView(viewHead("System", "gateway service, nightly pass, reflection, consolidation") + loading("system status"));
  try {
    const [gw, night, refl, cons] = await Promise.all([
      api("GET", "/api/gateway/status"),
      api("GET", "/api/nightly/status"),
      api("GET", "/api/reflect/status"),
      api("GET", "/api/consolidate/status"),
    ]);
    setView(viewHead("System", "gateway service, nightly pass, reflection, consolidation") +
      '<div class="grid-2">' +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Gateway</span><span class="spacer"></span>' +
      '<button class="btn small danger" id="sy-gw-restart">Restart</button></div><div class="panel-body"><dl class="kv">' +
      "<dt>Detected</dt><dd class='mono'>" + esc(gw.detected) + "</dd>" +
      "<dt>Installed</dt><dd class='mono'>" + esc(gw.installed || "no") + "</dd>" +
      "<dt>Running</dt><dd>" + (gw.running ? '<span class="pill" data-s="ok">running</span>' : '<span class="pill" data-s="warn">stopped</span>') + "</dd>" +
      "</dl></div></div>" +
      '<div class="panel"><div class="panel-head"><span class="panel-title">Nightly pass</span><span class="spacer"></span>' +
      '<button class="btn small' + (night.enabled ? " danger" : " primary") + '" id="sy-night-toggle">' + (night.enabled ? "Disable" : "Enable") + '</button></div>' +
      '<div class="panel-body"><dl class="kv">' +
      "<dt>Loop</dt><dd>" + (night.enabled ? '<span class="pill" data-s="ok">enabled</span>' : '<span class="pill" data-s="warn">off</span>') + "</dd>" +
      "<dt>Why</dt><dd class='mono'>" + esc(night.reason || "—") + "</dd>" +
      "<dt>Next run</dt><dd class='mono'>" + (night.next_run_ms ? fmtTime(night.next_run_ms) + " (" + relTime(night.next_run_ms) + ")" : "not scheduled") + "</dd>" +
      "<dt>Last pass</dt><dd class='mono' style='white-space:pre-wrap'>" + esc(night.last_summary || "no nightly passes yet") + "</dd>" +
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
    $("#sy-night-toggle").onclick = async () => {
      const enable = !night.enabled;
      const ok = await confirmDialog({
        title: (enable ? "Enable" : "Disable") + " nightly pass?",
        body: '<p class="m-sub">' + (enable
          ? "Writes <code class=\"inline\">enabled = true</code> to <code class=\"inline\">[nightly]</code> — the same flag <code class=\"inline\">/nightly on</code> writes."
          : "Writes <code class=\"inline\">enabled = false</code> to <code class=\"inline\">[nightly]</code>; it wins even with a model pin.") + "</p>",
        confirmLabel: enable ? "Enable" : "Disable",
        danger: !enable,
      });
      if (!ok) return;
      try { await api("POST", "/api/nightly/enabled", { enabled: enable, confirm: true }); toast("Nightly " + (enable ? "enabled" : "disabled"), "ok"); renderers.system(); }
      catch (e) { toast(e.message, "err"); }
    };
    $("#sy-gw-restart").onclick = async () => {
      const ok = await confirmDialog({ title: "Restart gateway?", body: '<p class="m-sub">Runs <code class="inline">pantheon gateway restart</code> in the background. <strong>This page will disconnect</strong> when the gateway restarts — reload the dashboard URL afterwards. Chat surfaces may drop briefly.</p>', confirmLabel: "Restart", danger: true });
      if (!ok) return;
      try { await api("POST", "/api/gateway/restart", { confirm: true }); toast("Restart requested — this page will disconnect", "ok"); }
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

/* ------------------------------------------------------------------ */
/* Swarm: launch a task across N subagents or picked profiles, with an  */
/* optional judge ruling whether the work is done. Built against the    */
/* mobile client's API contract                                         */
/* (pantheon-mobile/lib/services/pantheon_api.dart ~L954-993):          */
/*   POST /api/swarm {task, mode, subagent_count?, profiles?, judge}    */
/*     -> 201 {swarm_id, run_id, agents}                                */
/*   GET  /api/swarm/status?swarm=<id>                                  */
/*     -> {task, status, round, agents, verdict}                        */
/*   GET  /api/swarm/transcript?swarm=<id>&agent=<name>                 */
/*   POST /api/swarm/<id>/retry {feedback?} -> 200 {swarm_id, round}    */
/* The backend routes HAVE landed (see src/swarm.rs); the contract has  */
/* no list endpoint, so recently launched swarms are remembered         */
/* client-side in sessionStorage.                                       */
/* ------------------------------------------------------------------ */

const SWARM_MISSING =
  "Swarm not found — it may have been pruned, or the id is wrong. " +
  "Recently launched swarms are remembered in this tab's sessionStorage.";

function swarmIsMissing(e) {
  return !!e && (e.status === 404 || /not found/i.test(String(e.message || "")));
}

function swarmRecent() {
  try { return JSON.parse(sessionStorage.getItem("swarm_recent") || "[]"); }
  catch (e) { return []; }
}

function swarmRemember(entry) {
  const list = swarmRecent().filter((x) => x.id !== entry.id);
  list.unshift(entry);
  try { sessionStorage.setItem("swarm_recent", JSON.stringify(list.slice(0, 20))); }
  catch (e) { /* storage full/blocked: recent list is best-effort */ }
}

/* Normalize the status call's per-agent payload: accept an array of
   names/objects or a name->status map. */
function swarmAgentsOf(st) {
  const a = st.agents;
  if (Array.isArray(a)) {
    return a.map((x) => typeof x === "string"
      ? { name: x, status: "" }
      : { name: x.name || x.id || "agent", status: x.status || "" });
  }
  if (a && typeof a === "object") {
    return Object.keys(a).map((k) => {
      const v = a[k];
      return { name: k, status: typeof v === "string" ? v : ((v && v.status) || "") };
    });
  }
  return [];
}

function swarmTranscriptHtml(t) {
  if (t && Array.isArray(t.messages)) {
    if (!t.messages.length) return '<span class="text-faint">No transcript yet.</span>';
    return t.messages.map((m) =>
      '<div style="margin-bottom:10px"><div class="mono text-faint" style="font-size:11px;margin-bottom:2px">' +
      esc(m.role || m.author || "?") + "</div><div style=\"white-space:pre-wrap;font-size:13px\">" +
      esc(m.text || m.content || "") + "</div></div>"
    ).join("");
  }
  if (t && typeof t.transcript === "string") {
    return '<pre class="mono" style="white-space:pre-wrap;font-size:12px;line-height:1.6;margin:0">' +
      esc(t.transcript) + "</pre>";
  }
  return '<pre class="mono" style="white-space:pre-wrap;font-size:12px;margin:0">' +
    esc(JSON.stringify(t, null, 2)) + "</pre>";
}

/* ---------- team runs: a staged swarm reads as a multi-agent conversation ----------
   Every message is labeled with the speaking expert (avatar + name, like the
   HERMES / MEDUSA labels); handoffs between experts render as @-mention
   chips; the header shows the participant stack. The lead's messages are the
   primary thread — member runs are team activity, never user-facing. */

/* Avatar in the expert's own color when we have it, else the hash avatar. */
function expertAvatarHtml(x, cls) {
  const color = x && x.color;
  if (color && /^#[0-9a-fA-F]{6}$/.test(String(color))) {
    return '<span class="' + (cls || "av") + '" style="background:' + esc(color) +
      ';color:#fff">' + esc(initials((x && x.name) || "?")) + "</span>";
  }
  return avatarHtml((x && x.name) || "?", cls);
}

/* One @-mention chip for an expert name. */
function mentionHtml(name) {
  return '<span class="mention">@' + esc(name) + "</span>";
}

/* Split a combined swarm transcript into headed blocks:
   === Name (r_1) [status] [role] === ... plus the staged execution log. */
function parseTeamTranscript(text) {
  const blocks = [];
  const lines = String(text || "").split("\n");
  let cur = null;
  const head = /^=== (.+?) \((r_\d+)\) \[([^\]]+)\](?: \[([^\]]+)\])? ===$/;
  for (const line of lines) {
    const m = head.exec(line);
    if (m) {
      cur = { name: m[1], run: m[2], status: m[3], role: m[4] || "", log: false, body: [] };
      blocks.push(cur);
      continue;
    }
    if (/^=== staged execution log: /.test(line)) {
      cur = { name: "", log: true, title: line.replace(/^=== /, "").replace(/ ===$/, ""), body: [] };
      blocks.push(cur);
      continue;
    }
    if (cur) cur.body.push(line);
  }
  blocks.forEach((b) => { b.text = b.body.join("\n").replace(/^\n+|\s+$/g, ""); delete b.body; });
  return blocks;
}

/* Wrap every known expert name in a log line with an @-mention chip. */
function chipNamesHtml(line, names) {
  let out = esc(line);
  const sorted = names.slice().sort((a, b) => b.length - a.length);
  sorted.forEach((n) => {
    const needle = esc(n);
    if (!needle || out.indexOf(needle) < 0) return;
    out = out.split(needle).join('<span class="mention">@' + needle + "</span>");
  });
  return out;
}

function expertBlockHtml(block, isLead) {
  return '<div class="tm-msg' + (isLead ? " lead" : "") + '">' +
    '<div class="tm-who">' + avatarHtml(block.name, "av") +
    '<span class="tm-name">' + esc(block.name) + "</span>" +
    (isLead ? '<span class="tm-tag">lead</span>' : "") +
    (block.status ? '<span class="text-faint mono" style="font-size:11px">' + esc(block.status) + "</span>" : "") +
    "</div>" +
    (block.text
      ? '<div class="tm-body">' + esc(block.text) + "</div>"
      : '<div class="tm-body text-faint">No output yet.</div>') +
    "</div>";
}

function handoffHtml(fromNames, toNames, label) {
  return '<div class="tm-handoff"><span class="text-faint">' + esc(label || "handoff") + "</span> " +
    fromNames.map(mentionHtml).join(" ") +
    ' <span aria-hidden="true">\u2192</span> ' +
    toNames.map(mentionHtml).join(" ") + "</div>";
}

function teamTranscriptHtml(text, st) {
  const staged = st.staged || {};
  const stages = staged.stages || [];
  const blocks = parseTeamTranscript(text);
  const byName = {};
  blocks.forEach((b) => { if (!b.log) byName[b.name] = b; });
  const knownNames = Object.keys(byName);
  let html = "";
  // The lead's block is the primary thread, first.
  const leadBlock = blocks.find((b) => !b.log && /lead/i.test(b.role || ""));
  if (leadBlock) html += expertBlockHtml(leadBlock, true);
  // Then each stage in plan order: member sections, then the handoff row.
  stages.forEach((sg, i) => {
    const members = (sg.members || []).filter((n) => n !== (leadBlock && leadBlock.name));
    const shown = members.filter((n) => byName[n]).map((n) => byName[n]);
    if (!shown.length && !members.length) return;
    html += '<div class="tm-stage"><span class="stage-pill' +
      (i < staged.stage_index ? " done" : i === staged.stage_index ? " current" : " todo") + '">' +
      "Stage " + (i + 1) + " \u00b7 " + esc(sg.name || "") + "</span></div>";
    shown.forEach((b) => { html += expertBlockHtml(b, false); });
    // Any agent blocks not claimed by a stage (shouldn't happen) stay visible.
    if (i === stages.length - 1) {
      const claimed = {};
      stages.forEach((s2) => (s2.members || []).forEach((n) => { claimed[n] = 1; }));
      if (leadBlock) claimed[leadBlock.name] = 1;
      blocks.forEach((b) => {
        if (!b.log && !claimed[b.name]) html += expertBlockHtml(b, false);
      });
    }
    const next = stages[i + 1];
    if (next && members.length && (next.members || []).length) {
      html += handoffHtml(members, next.members, "stage " + (i + 1) + " hands off to stage " + (i + 2));
    }
    if (sg.loop_back_to != null && stages[sg.loop_back_to]) {
      html += '<div class="tm-handoff"><span class="text-faint">on verification failure loops back to</span> ' +
        mentionHtml("stage " + (sg.loop_back_to + 1) + " \u00b7 " + stages[sg.loop_back_to].name) + "</div>";
    }
  });
  // The staged execution log: system rows with @-mention chips.
  blocks.forEach((b) => {
    if (!b.log || !b.text) return;
    html += '<div class="tm-stage"><span class="stage-pill">execution log</span></div>';
    b.text.split("\n").forEach((line) => {
      if (!line.trim()) return;
      const cls = /^escalation:/.test(line) ? " err" : /^review:/.test(line) ? " warn" : "";
      html += '<div class="tm-sys' + cls + '">' + chipNamesHtml(line, knownNames) + "</div>";
    });
  });
  return html || '<span class="text-faint">No transcript yet.</span>';
}

/* Participant stack for a team-run header: lead first, then unique members. */
function teamStackHtml(st) {
  const staged = st.staged || {};
  const stages = staged.stages || [];
  const leadAgent = (st.agents || []).find((a) => a.lead);
  const names = [];
  const push = (n) => { if (n && names.indexOf(n) < 0) names.push(n); };
  if (leadAgent) push(leadAgent.name);
  stages.forEach((s) => (s.members || []).forEach(push));
  const stack = names.slice(0, 8).map((n) => avatarHtml(n)).join("") +
    (names.length > 8 ? '<span class="av more">+' + (names.length - 8) + "</span>" : "");
  return '<span class="av-stack">' + (stack || avatarHtml("?")) + "</span>" +
    '<span style="margin-left:8px;font-size:13px">' + names.map(esc).join(", ") + "</span>";
}

function stagePillsHtml(staged) {
  const stages = staged.stages || [];
  return '<div class="stage-pills">' + stages.map((s, i) => {
    const cls = i < staged.stage_index ? " done" : i === staged.stage_index ? " current" : " todo";
    return '<span class="stage-pill' + cls + '">' + (i + 1) + " \u00b7 " + esc(s.name || "") + "</span>";
  }).join("") + "</div>";
}

renderers.teamRun = async function (id, st) {
  const my = ++swarmPoll;
  const staged = st.staged || {};
  setView(viewHead("Team run", "") +
    '<div class="insp-head"><a class="btn small" href="#/swarm">' + icon("back", 14) + " Swarms</a>" +
    '<h1 class="view-title" style="font-size:18px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:60%">' +
    esc(staged.team || st.task || id) + '</h1><span class="mono text-faint">' + esc(id) + "</span></div>" +
    (staged.escalation
      ? '<div class="panel" style="border-color:var(--red-border)"><div class="panel-body">' +
        '<span class="pill" data-s="err">escalated</span>' +
        '<div style="margin-top:8px;font-size:13px;white-space:pre-wrap">' + esc(staged.escalation) + "</div></div></div>"
      : "") +
    '<div class="panel"><div class="panel-body">' +
    '<div style="display:flex;align-items:center;flex-wrap:wrap;gap:8px">' + teamStackHtml(st) + "</div>" +
    '<div style="display:flex;align-items:center;gap:10px;margin-top:10px;flex-wrap:wrap">' +
    '<span class="pill" data-s="accent">' + esc(String(staged.topology || "").replace(/_/g, " ")) + "</span>" +
    '<span class="text-faint" style="font-size:12px">' + esc(st.status || "running") +
    (staged.topology === "review_loop"
      ? " \u00b7 review iterations " + staged.review_iterations + "/" + staged.max_review_iterations
      : "") + "</span>" +
    '<span class="spacer"></span><button class="btn small" id="tm-refresh">Refresh</button></div>' +
    '<div style="margin-top:10px">' + stagePillsHtml(staged) + "</div>" +
    "</div></div>" +
    '<div class="panel" style="margin-top:16px"><div class="panel-head"><span class="panel-title">Team transcript</span>' +
    '<span class="spacer"></span><span class="text-faint" style="font-size:11px">lead speaks to you \u00b7 members report to the lead</span></div>' +
    '<div class="panel-body" id="tm-transcript">' + loading("transcript") + "</div></div>");
  $("#tm-refresh").onclick = () => renderers.swarmDetail(id);
  try {
    const t = await api("GET", "/api/swarm/transcript?swarm=" + encodeURIComponent(id));
    const box = $("#tm-transcript");
    if (box) box.innerHTML = teamTranscriptHtml(t && t.transcript, st);
  } catch (e) {
    const box = $("#tm-transcript");
    if (box) box.innerHTML = errorState(swarmIsMissing(e) ? SWARM_MISSING : e.message, false);
  }
  /* Keep the run live while it is working. */
  if (st.status === "running") {
    setTimeout(() => {
      if (my === swarmPoll && state.route === "swarm" && state.param === id) renderers.swarmDetail(id);
    }, 5000);
  }
};

renderers.swarm = async function () {
  if (state.param) return renderers.swarmDetail(state.param);
  setView(viewHead("Swarm", "split a task across subagents, with an optional judge") +
    '<div id="sw-view">' + loading("swarm") + "</div>");
  let profiles = [];
  try {
    const cfg = await api("GET", "/api/config");
    profiles = Object.keys((cfg.values || {}).agents || {}).sort();
  } catch (e) { /* the launch form still renders without the profile list */ }
  const recent = swarmRecent();
  const form =
    '<div class="panel"><div class="panel-head"><span class="panel-title">Launch swarm</span></div>' +
    '<div class="panel-body">' +
    '<label class="m-sub" for="sw-task" style="display:block;margin-bottom:6px">Task</label>' +
    '<textarea class="input" id="sw-task" rows="4" style="width:100%;box-sizing:border-box" ' +
    'placeholder="Describe the task for the swarm\u2026"></textarea>' +
    '<div style="display:flex;gap:18px;flex-wrap:wrap;margin-top:12px;align-items:flex-start">' +
    '<div><div class="m-sub" style="margin-bottom:6px">Mode</div>' +
    '<label style="margin-right:12px"><input type="radio" name="sw-mode" value="count" checked> count</label>' +
    '<label><input type="radio" name="sw-mode" value="profiles"> profiles</label></div>' +
    '<div id="sw-count-wrap"><div class="m-sub" style="margin-bottom:6px">Subagents</div>' +
    '<input type="number" class="input" id="sw-count" min="1" max="16" value="3" style="width:76px"></div>' +
    '<div><div class="m-sub" style="margin-bottom:6px">Judge</div>' +
    '<label><input type="checkbox" id="sw-judge" checked> judge verdict</label></div>' +
    "</div>" +
    '<div id="sw-profiles-wrap" style="display:none;margin-top:12px"><div class="m-sub" style="margin-bottom:6px">Profiles</div>' +
    (profiles.length
      ? profiles.map((p) => '<label style="margin-right:12px"><input type="checkbox" name="sw-profile" value="' +
        esc(p) + '"> ' + esc(p) + "</label>").join("")
      : '<span class="text-faint">No [agents.*] profiles declared.</span>') +
    "</div>" +
    '<div class="btn-row" style="margin-top:14px"><button class="btn primary" id="sw-launch">Launch swarm</button></div>' +
    '<div id="sw-err" style="margin-top:10px"></div>' +
    "</div></div>";
  const list =
    '<div class="panel" style="margin-top:16px"><div class="panel-head"><span class="panel-title">Recent swarms</span>' +
    '<span class="spacer"></span><span class="text-faint" style="font-size:11px">remembered in this tab</span></div>' +
    '<div class="panel-body">' +
    (recent.length
      ? '<div class="qa-grid">' + recent.map((r) =>
        '<a class="card-link card" href="#/swarm/' + encodeURIComponent(r.id) + '" style="text-decoration:none">' +
        '<div class="card-body" style="padding:14px 18px"><div class="agent-name">' +
        esc((r.task || "").slice(0, 80) || r.id) + '</div>' +
        '<div class="agent-sub">' + esc(r.id) + (r.created ? " \u00b7 " + esc(r.created) : "") + "</div></div></a>"
      ).join("") + "</div>"
      : emptyState("No swarms launched yet", "Launch one above — it will be remembered here for this tab.")) +
    "</div></div>";
  $("#sw-view").innerHTML = form + list;

  $$('input[name="sw-mode"]').forEach((r) => {
    r.onchange = () => {
      const byProfiles = document.querySelector('input[name="sw-mode"]:checked').value === "profiles";
      $("#sw-count-wrap").style.display = byProfiles ? "none" : "";
      $("#sw-profiles-wrap").style.display = byProfiles ? "" : "none";
    };
  });
  $("#sw-launch").onclick = async () => {
    const task = $("#sw-task").value.trim();
    if (!task) { toast("Describe the task first", "err"); return; }
    const mode = document.querySelector('input[name="sw-mode"]:checked').value;
    const judge = $("#sw-judge").checked;
    const body = { task: task, mode: mode, judge: judge };
    if (mode === "count") {
      body.subagent_count = Math.max(1, parseInt($("#sw-count").value, 10) || 3);
    } else {
      body.profiles = $$('input[name="sw-profile"]:checked').map((c) => c.value);
      if (!body.profiles.length) { toast("Pick at least one profile", "err"); return; }
    }
    const btn = $("#sw-launch");
    btn.disabled = true;
    btn.textContent = "Launching\u2026";
    try {
      const res = await api("POST", "/api/swarm", body);
      const id = res.swarm_id || res.id;
      swarmRemember({ id: id, task: task, created: new Date().toLocaleString() });
      toast("Swarm launched", "ok");
      location.hash = "#/swarm/" + encodeURIComponent(id);
    } catch (e) {
      $("#sw-err").innerHTML = swarmIsMissing(e) ? errorState(SWARM_MISSING, false) : errorState(e.message, false);
      btn.disabled = false;
      btn.textContent = "Launch swarm";
    }
  };
};

/* Poll guard: every detail render bumps the counter, so stale timers die. */
let swarmPoll = 0;

renderers.swarmDetail = async function (id) {
  const my = ++swarmPoll;
  setView(viewHead("Swarm", "") + loading("swarm status"));
  let st;
  try {
    st = await api("GET", "/api/swarm/status?swarm=" + encodeURIComponent(id));
  } catch (e) {
    const back = '<div class="insp-head"><a class="btn small" href="#/swarm">' + icon("back", 14) + " Swarms</a></div>";
    if (swarmIsMissing(e)) {
      setView(viewHead("Swarm", "") + back + emptyState("Swarm backend unavailable", SWARM_MISSING));
    } else {
      setView(viewHead("Swarm", "") + back + errorState(e.message, true));
      const rb = $("[data-retry]");
      if (rb) rb.onclick = () => renderers.swarmDetail(id);
    }
    return;
  }
  const agents = swarmAgentsOf(st);
  const v = st.verdict;
  /* Staged swarms render as a team run: a multi-agent conversation with
     per-expert attribution, handoff chips, and the participant stack. */
  if (st.staged) return renderers.teamRun(id, st);
  const isDone = v === "done" || (v && v.done === true);
  let banner;
  if (isDone) {
    banner = '<div class="panel"><div class="panel-body"><span class="pill" data-s="ok">verdict: done</span>' +
      '<span class="text-faint" style="margin-left:8px">the judge accepted the work</span></div></div>';
  } else if (v) {
    banner = '<div class="panel"><div class="panel-head"><span class="panel-title">Judge verdict</span>' +
      '<span class="spacer"></span><button class="btn small primary" id="sw-retry">Retry round</button></div>' +
      '<div class="panel-body"><span class="pill" data-s="warn">not done</span>' +
      '<div style="margin-top:8px;white-space:pre-wrap;font-size:13px">' +
      esc(typeof v === "string" ? v : JSON.stringify(v, null, 2)) + "</div></div></div>";
  } else {
    banner = '<div class="panel"><div class="panel-body"><span class="pill" data-s="running">' +
      esc(st.status || "running") + '</span><span class="text-faint" style="margin-left:8px">awaiting judge verdict' +
      (st.round != null ? " \u00b7 round " + esc(String(st.round)) : "") + '</span><span class="spacer"></span>' +
      '<button class="btn small" id="sw-refresh">Refresh</button></div></div>';
  }
  const cards = agents.length
    ? '<div class="qa-grid">' + agents.map((a) =>
      '<div class="card" data-agent="' + esc(a.name) + '" style="cursor:pointer" role="button" tabindex="0">' +
      '<div class="card-body" style="padding:14px 18px"><div class="agent-name">' + esc(a.name) + "</div>" +
      '<div class="agent-sub">' + esc(a.status || "\u2014") + "</div></div></div>"
    ).join("") + "</div>"
    : emptyState("No agents yet", "The swarm has not spawned any agents.");
  setView(
    viewHead("Swarm", "") +
    '<div class="insp-head"><a class="btn small" href="#/swarm">' + icon("back", 14) + " Swarms</a>" +
    '<h1 class="view-title" style="font-size:18px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:60%">' +
    esc(st.task || id) + '</h1><span class="mono text-faint">' + esc(id) + "</span></div>" +
    banner +
    '<div class="panel" style="margin-top:16px"><div class="panel-head"><span class="panel-title">Agents</span>' +
    '<span class="spacer"></span><span class="text-faint" style="font-size:11px">tap a card for its transcript</span></div>' +
    '<div class="panel-body">' + cards + "</div></div>" +
    '<div id="sw-transcript" style="margin-top:16px"></div>'
  );
  const showTranscript = async (name) => {
    const box = $("#sw-transcript");
    box.innerHTML = '<div class="panel"><div class="panel-head"><span class="panel-title">Transcript \u00b7 ' +
      esc(name) + '</span><span class="spacer"></span><button class="btn small" id="sw-ts-close">Close</button></div>' +
      '<div class="panel-body">' + loading("transcript") + "</div></div>";
    $("#sw-ts-close").onclick = () => { box.innerHTML = ""; };
    try {
      const t = await api("GET", "/api/swarm/transcript?swarm=" + encodeURIComponent(id) +
        "&agent=" + encodeURIComponent(name));
      box.querySelector(".panel-body").innerHTML = swarmTranscriptHtml(t);
    } catch (e) {
      box.querySelector(".panel-body").innerHTML =
        errorState(swarmIsMissing(e) ? SWARM_MISSING : e.message, false);
    }
  };
  $$("#view [data-agent]").forEach((c) => {
    const open = () => showTranscript(c.dataset.agent);
    c.onclick = open;
    c.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); } };
  });
  const retryBtn = $("#sw-retry");
  if (retryBtn) retryBtn.onclick = async () => {
    const ok = await confirmDialog({
      title: "Retry swarm round?",
      body: '<p class="m-sub">Optional feedback for the next round \u2014 blank uses the judge\u2019s notes.</p>' +
        '<textarea class="input" id="sw-feedback" rows="3" style="width:100%;box-sizing:border-box"></textarea>',
      confirmLabel: "Retry",
    });
    if (!ok) return;
    const fb = ($("#sw-feedback") || { value: "" }).value.trim();
    try {
      const r = await api("POST", "/api/swarm/" + encodeURIComponent(id) + "/retry", fb ? { feedback: fb } : {});
      toast("Retry started" + (r.round != null ? " \u2014 round " + r.round : ""), "ok");
      renderers.swarmDetail(id);
    } catch (e) { toast(swarmIsMissing(e) ? SWARM_MISSING : e.message, "err"); }
  };
  const refreshBtn = $("#sw-refresh");
  if (refreshBtn) refreshBtn.onclick = () => renderers.swarmDetail(id);
  /* Keep the cards live while the verdict is pending. */
  if (!v) {
    setTimeout(() => {
      if (my === swarmPoll && state.route === "swarm" && state.param === id) renderers.swarmDetail(id);
    }, 5000);
  }
};

/* ---------------- boot ---------------- */
/* ---------------- profiles: persona files ---------------- */
const PROFILE_FILES = [
  ["soul", "SOUL", "Persona — who this agent is"],
  ["user", "USER", "User context — who it serves"],
  ["agents", "AGENTS", "Instructions — how it works"],
];

renderers.profiles = async function () {
  if (state.param) return renderers.profileEditor();
  setView(viewHead("Profiles", "agent identities and their persona files") + '<div id="pflist">' + loading("profiles") + "</div>");
  try {
    const cfg = await api("GET", "/api/config");
    const values = cfg.values || {};
    const agents = values.agents || {};
    const active = values.agent || "";
    const names = Object.keys(agents).sort();
    if (!names.length) {
      $("#pflist").innerHTML = emptyState("No profiles declared", "Add an [agents.<name>] table under Config to give Pantheon a named identity.");
      return;
    }
    $("#pflist").innerHTML = '<div class="qa-grid">' + names.map((n) => {
      const p = agents[n] || {};
      const display = p.display_name || n;
      const initial = (display.trim().charAt(0) || "?").toUpperCase();
      return '<a class="card-link card" href="#/profiles/' + encodeURIComponent(n) + '" style="text-decoration:none">' +
        '<div class="card-body" style="padding:16px 18px"><div style="display:flex;align-items:center;gap:14px">' +
        '<span class="agent-avatar" style="width:44px;height:44px;font-size:18px">' + esc(initial) + "</span>" +
        '<span><span class="agent-name">' + esc(display) + "</span><br>" +
        '<span class="agent-sub mono">' + esc(n) + "</span></span>" +
        (n === active ? '<span style="margin-left:auto">' + statusPill("true") + "</span>" : "") +
        "</div></div></a>";
    }).join("") + "</div>";
  } catch (e) {
    $("#pflist").innerHTML = errorState(e.message, true);
    const rb = $("[data-retry]");
    if (rb) rb.onclick = () => renderers.profiles();
  }
};

function profileFileCard(name, kind, label, sub, file) {
  const content = file.content || "";
  const preview = content.split("\n").slice(0, 3).join("\n") || "— empty —";
  const pathLine = file.path
    ? '<div class="mono text-faint" style="font-size:11px;margin-top:8px;word-break:break-all">' + esc(file.path) + "</div>"
    : '<div class="text-faint" style="font-size:11px;margin-top:8px">Not set — saving creates it under the profile.</div>';
  return '<div class="panel" id="pf-' + kind + '"><div class="panel-head">' +
    '<span class="panel-title">' + esc(label) + '</span><span class="spacer"></span>' +
    '<button class="btn small" data-edit="' + kind + '">Edit</button></div>' +
    '<div class="panel-body"><div class="view-sub" style="margin-bottom:10px">ACCESS WITH CARE · ' + esc(sub) + "</div>" +
    '<pre class="mono" data-preview style="white-space:pre-wrap;font-size:12px;line-height:1.6;max-height:120px;overflow:hidden;margin:0">' + esc(preview) + "</pre>" +
    pathLine + "</div></div>";
}

renderers.profileEditor = async function () {
  const name = state.param || "";
  setView(viewHead("Profile", "") + loading("persona files"));
  try {
    const cfg = await api("GET", "/api/config");
    const values = cfg.values || {};
    const agents = values.agents || {};
    const p = agents[name];
    if (!p) {
      setView(viewHead("Profile", "") + errorState("No [agents." + name + "] profile declared.", false));
      return;
    }
    const active = values.agent || "";
    const display = p.display_name || name;
    const initial = (display.trim().charAt(0) || "?").toUpperCase();
    const files = await api("GET", "/api/profiles/" + encodeURIComponent(name) + "/files");
    const head =
      '<div class="insp-head"><a class="btn small" href="#/profiles">' + icon("back", 14) + " Profiles</a>" +
      '<span class="agent-avatar" style="width:48px;height:44px;font-size:20px">' + esc(initial) + "</span>" +
      '<h1 class="view-title">' + esc(display) + '</h1><span class="mono text-faint">' + esc(name) + "</span>" +
      (name === active ? statusPill("true") : "") +
      '<span class="spacer"></span>' +
      (name === active
        ? ""
        : '<button class="btn small" id="pf-activate">Set as active</button>' +
          '<button class="btn small danger" id="pf-delete">Delete</button>') +
      "</div>";
    setView(head + '<div class="view-sub" style="margin-bottom:14px">These files are injected into the agent\'s prompt every turn. Edit with care.</div>' +
      '<div class="qa-grid">' +
      PROFILE_FILES.map(([kind, label, sub]) => profileFileCard(name, kind, label, sub, files[kind] || {})).join("") +
      "</div>");
    const actBtn = $("#pf-activate");
    if (actBtn) actBtn.onclick = async () => {
      try {
        await api("PUT", "/api/config", { changes: { agent: name }, confirm: true });
        toast(name + " is now the active profile", "ok");
        renderers.profileEditor();
      } catch (e) { toast(e.message, "err"); }
    };
    const delBtn = $("#pf-delete");
    if (delBtn) delBtn.onclick = async () => {
      const ok = await confirmDialog({
        title: "Delete profile " + name + "?",
        body: '<p class="m-sub">Removes the <code class="inline">[agents.' + esc(name) + "]</code> table from config.toml. This cannot be undone.</p>",
        confirmLabel: "Delete", danger: true,
      });
      if (!ok) return;
      try {
        await api("DELETE", "/api/profiles/" + encodeURIComponent(name));
        toast("Profile " + name + " deleted", "ok");
        location.hash = "#/profiles";
      } catch (e) { toast(e.message, "err"); }
    };
    $$("#view [data-edit]").forEach((b) => {
      b.onclick = () => profileEditFile(name, b.dataset.edit, files[b.dataset.edit] || {});
    });
  } catch (e) {
    setView(viewHead("Profile", "") + errorState(e.message, true));
    const rb = $("[data-retry]");
    if (rb) rb.onclick = () => renderers.profileEditor();
  }
};

async function profileEditFile(name, kind, file) {
  const panel = $("#pf-" + kind);
  if (!panel) return;
  const label = (PROFILE_FILES.find(([k]) => k === kind) || [kind, kind])[1];
  const body = panel.querySelector(".panel-body");
  const original = file.content || "";
  body.innerHTML =
    '<textarea class="input" id="pf-ta" rows="18" spellcheck="false" style="width:100%;box-sizing:border-box">' + esc(original) + "</textarea>" +
    '<div class="btn-row" style="margin-top:10px;display:flex;gap:8px">' +
    '<button class="btn primary" id="pf-save">Save</button>' +
    '<button class="btn" id="pf-discard">Discard</button>' +
    '<span class="view-sub" id="pf-dirty"></span></div>';
  const ta = $("#pf-ta");
  const dirty = $("#pf-dirty");
  ta.addEventListener("input", () => {
    dirty.textContent = ta.value !== original ? "unsaved changes" : "";
  });
  $("#pf-discard").onclick = () => renderers.profileEditor();
  $("#pf-save").onclick = async () => {
    const btn = $("#pf-save");
    btn.disabled = true;
    btn.textContent = "Saving…";
    try {
      await api("PUT", "/api/profiles/" + encodeURIComponent(name) + "/files", { file: kind, content: ta.value });
      toast(label + " saved", "ok");
      renderers.profileEditor();
    } catch (e) {
      toast(e.message, "err");
      btn.disabled = false;
      btn.textContent = "Save";
    }
  };
  ta.focus();
}

/* ---------------- experts (Teams | Experts tabs) ---------------- */
/* The /api/teams and /api/experts routes are live. If they 404, each tab
   degrades to a "backend unavailable" empty state instead of breaking the
   page, same pattern as the swarm view. */
const TEAMS_MISSING =
  "Teams backend unavailable \u2014 the /api/teams routes have not landed yet. " +
  "This UI follows the /api/teams contract and will light up when they do.";
const EXPERTS_MISSING =
  "Experts backend unavailable \u2014 the /api/experts routes have not landed yet. " +
  "This UI follows the /api/experts contract and will light up when they do.";

function xIsMissing(e) {
  return !!e && (e.status === 404 || /not found/i.test(String(e.message || "")));
}

function initials(name) {
  const parts = String(name || "?").trim().split(/[\s._-]+/).filter(Boolean);
  return parts.slice(0, 2).map((p) => p.charAt(0).toUpperCase()).join("") || "?";
}
function avColor(name) {
  let h = 0;
  const s = String(name || "?");
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) % 6;
  return h;
}
function avatarHtml(name, cls) {
  return '<span class="' + (cls || "av") + '" data-c="' + avColor(name) + '">' + esc(initials(name)) + "</span>";
}
/* Accept an array of names/objects or a name->role map, like swarmAgentsOf. */
function teamMembersOf(t) {
  const m = t.members || t.agents;
  if (Array.isArray(m)) {
    return m.map((x) => typeof x === "string"
      ? { name: x, role: "" }
      : { name: x.name || x.id || "member", role: x.role || x.title || "" });
  }
  if (m && typeof m === "object") {
    return Object.keys(m).map((k) => ({ name: k, role: String(m[k] || "") }));
  }
  return [];
}

function teamCardHtml(t) {
  const id = t.id || "";
  const members = teamMembersOf(t);
  const stack = members.slice(0, 5).map((m) => avatarHtml(m.name)).join("") +
    (members.length > 5 ? '<span class="av more">+' + (members.length - 5) + "</span>" : "");
  return '<article class="team-card" data-team="' + esc(id) + '" tabindex="0" role="button" ' +
    'aria-label="' + esc(t.name || "team") + ' details">' +
    '<div class="team-band"><span class="av-stack">' + (stack || avatarHtml("?")) + "</span></div>" +
    '<div class="team-body"><div class="team-name">' + esc(t.name || id || "Untitled team") + "</div>" +
    (t.description
      ? '<p class="team-desc clamp2">' + esc(t.description) + "</p>"
      : '<p class="team-desc clamp2 text-faint">No description.</p>') +
    '<div class="team-foot"><span class="team-count">' + members.length +
    (members.length === 1 ? " member" : " members") + '</span>' +
    '<button class="btn small primary" data-use="' + esc(id) + '">Use team</button></div></div></article>';
}

async function useTeamNow(id, task, closeModal) {
  const res = await api("POST", "/api/teams/" + encodeURIComponent(id) + "/use", task ? { task: task } : {});
  const sid = res.swarm_id || res.id;
  if (!sid) throw { message: "the /use endpoint returned no swarm id" };
  if (closeModal) closeModal();
  toast("Team launched", "ok");
  location.hash = "#/swarm/" + encodeURIComponent(sid);
}

/* Enriched roster from GET /api/teams/:id: members carry their expert
   identity (or null when the expert was deleted). */
function teamRosterOf(t) {
  const m = t.members;
  if (!Array.isArray(m)) return teamMembersOf(t);
  return m.map((x) => ({
    name: (x.expert && x.expert.name) || x.expert_id || "member",
    role: x.role || "",
    expert: x.expert || null,
    unresolved: !x.expert,
  }));
}

function topoLabel(topo) {
  const s = String(topo || "").replace(/_/g, " ");
  return s ? s.charAt(0).toUpperCase() + s.slice(1) : "Team";
}

/* Topology badge, lead row, and the ordered stages with their handoff
   contracts — how the team actually executes. */
function teamPlanHtml(t) {
  const stages = t.stages || [];
  const byId = {};
  (t.members || []).forEach((m) => { if (m.expert_id) byId[m.expert_id] = m; });
  const lead = t.lead;
  let html = '<div class="section-label" style="margin-top:0">How it runs</div>' +
    '<div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;margin-bottom:6px">' +
    '<span class="pill" data-s="accent">' + esc(topoLabel(t.topology)) + "</span>" +
    (lead ? '<span style="display:inline-flex;align-items:center;gap:8px">' + expertAvatarHtml(lead, "av") +
      '<span><span class="roster-name">' + esc(lead.name || "Lead") + '</span> <span class="tm-tag">lead</span><br>' +
      '<span class="roster-role">only the lead talks to you</span></span></span>'
      : "") +
    "</div>";
  if (!stages.length) return html;
  html += stages.map((s, i) => {
    const mems = (s.members || []).map((id) => {
      const m = byId[id];
      const x = (m && m.expert) || { name: id };
      return '<span class="chip">' + expertAvatarHtml(x, "av xs") + esc(x.name || id) + "</span>";
    }).join(" ");
    return '<div class="tm-stage-block"><div class="tm-stage"><span class="stage-pill">' +
      "Stage " + (i + 1) + " \u00b7 " + esc(s.name || "") + "</span></div>" +
      (mems ? '<div style="margin:6px 0;display:flex;gap:6px;flex-wrap:wrap">' + mems + "</div>" : "") +
      (s.input_contract ? '<div class="contract"><span class="contract-k">in</span> ' + esc(s.input_contract) + "</div>" : "") +
      (s.output_contract ? '<div class="contract"><span class="contract-k">out</span> ' + esc(s.output_contract) + "</div>" : "") +
      (s.loop_back_to
        ? '<div class="tm-handoff"><span class="text-faint">on verification failure loops back to stage</span> ' +
          '<span class="chip">' + esc(s.loop_back_to) + "</span></div>"
        : "") +
      "</div>";
  }).join("");
  return html;
}

function openTeamDetail(id) {
  const root = $("#modal-root");
  const close = () => { root.innerHTML = ""; };
  root.innerHTML = '<div class="modal-veil"><div class="modal" role="dialog" aria-modal="true" aria-label="Team details">' +
    loading("team") + "</div></div>";
  $(".modal-veil", root).addEventListener("mousedown", (e) => { if (e.target.classList.contains("modal-veil")) close(); });
  (async () => {
    let t;
    try {
      t = await api("GET", "/api/teams/" + encodeURIComponent(id));
    } catch (e) {
      $(".modal", root).innerHTML = "<h3>Team</h3>" + errorState(e.message, false) +
        '<div class="m-actions"><button class="btn" data-x>Close</button></div>';
      $("[data-x]", root).onclick = close;
      return;
    }
    t = t.team || t;
    const members = teamRosterOf(t);
    const brief = t.task_brief || t.brief || t.brief_template || "";
    $(".modal", root).innerHTML =
      "<h3>" + esc(t.name || id || "Team") + "</h3>" +
      (t.description ? '<p class="m-sub">' + esc(t.description) + "</p>" : "") +
      teamPlanHtml(t) +
      '<div class="section-label">Members \u00b7 ' + members.length + "</div>" +
      (members.length
        ? '<ul class="roster">' + members.map((m) =>
          "<li>" + expertAvatarHtml(m.expert || { name: m.name }) +
          '<span><span class="roster-name">' + esc(m.name) + "</span>" +
          (m.unresolved ? ' <span class="pill" data-s="warn">unresolved</span>' : "") +
          (m.role ? '<br><span class="roster-role">' + esc(m.role) + "</span>" : "") + "</span></li>"
        ).join("") + "</ul>"
        : '<p class="view-sub">No members listed.</p>') +
      (brief
        ? '<div class="section-label">Task brief template</div><pre class="brief-block">' + esc(brief) + "</pre>"
        : "") +
      '<div class="field" style="margin-top:16px"><label for="td-task">Task (optional)</label>' +
      '<textarea class="input" id="td-task" rows="2" style="box-sizing:border-box" spellcheck="false" ' +
      'placeholder="Override the task brief\u2026"></textarea></div>' +
      '<div id="td-err"></div>' +
      '<div class="m-actions"><button class="btn" data-x>Cancel</button>' +
      '<button class="btn primary" data-use-team>Use team</button></div>';
    $("[data-x]", root).onclick = close;
    $("[data-use-team]", root).onclick = async () => {
      const btn = $("[data-use-team]", root);
      const task = $("#td-task", root).value.trim();
      btn.disabled = true;
      btn.textContent = "Launching\u2026";
      try {
        await useTeamNow(id, task, close);
      } catch (e) {
        $("#td-err", root).innerHTML = errorState(e.message, false);
        btn.disabled = false;
        btn.textContent = "Use team";
      }
    };
  })();
}

async function loadTeams() {
  const body = $("#x-body");
  body.innerHTML = loading("teams");
  try {
    const data = await api("GET", "/api/teams");
    const teams = data.teams || [];
    if (!teams.length) {
      body.innerHTML = emptyState("No teams yet", "Define teams on the backend and they will appear here.");
      return;
    }
    body.innerHTML = '<div class="team-grid">' + teams.map(teamCardHtml).join("") + "</div>";
    $$("#x-body .team-card").forEach((card) => {
      const id = card.dataset.team;
      card.addEventListener("click", (e) => {
        if (e.target.closest("[data-use]")) return;
        openTeamDetail(id);
      });
      card.addEventListener("keydown", (e) => {
        if (e.key === "Enter" && !e.target.closest("[data-use]")) openTeamDetail(id);
      });
    });
    $$("#x-body [data-use]").forEach((b) => {
      b.addEventListener("click", async (e) => {
        e.stopPropagation();
        b.disabled = true;
        const label = b.textContent;
        b.textContent = "Launching\u2026";
        try {
          await useTeamNow(b.dataset.use, "");
        } catch (err) {
          toast(err.message, "err");
          b.disabled = false;
          b.textContent = label;
        }
      });
    });
  } catch (e) {
    if (xIsMissing(e)) {
      body.innerHTML = emptyState("No teams backend yet", TEAMS_MISSING);
    } else {
      body.innerHTML = errorState(e.message, true);
      const rb = $("[data-retry]", body);
      if (rb) rb.onclick = loadTeams;
    }
  }
}

async function loadExperts() {
  const body = $("#x-body");
  body.innerHTML = loading("experts");
  try {
    const data = await api("GET", "/api/experts");
    const experts = data.experts || [];
    if (!experts.length) {
      body.innerHTML = emptyState("No experts yet", "Define experts on the backend and they will appear here.");
      return;
    }
    body.innerHTML = '<div class="team-grid">' + experts.map((x) => {
      const id = x.id || "";
      const name = x.name || id || "Expert";
      return '<article class="team-card expert-card"><div class="team-body">' +
        '<div class="expert-top">' + avatarHtml(name, "av xl") +
        "<div><div class=\"team-name\">" + esc(name) + "</div>" +
        (x.persona ? '<div class="agent-sub">' + esc(x.persona) + "</div>" : "") + "</div></div>" +
        (x.description
          ? '<p class="team-desc clamp2">' + esc(x.description) + "</p>"
          : '<p class="team-desc clamp2 text-faint">No description.</p>') +
        '<div class="team-foot"><span></span>' +
        '<button class="btn small primary" data-use-expert="' + esc(id) + '">Use expert</button>' +
        "</div></div></article>";
    }).join("") + "</div>";
    $$("#x-body [data-use-expert]").forEach((b) => {
      b.addEventListener("click", async () => {
        const id = b.dataset.useExpert;
        b.disabled = true;
        const label = b.textContent;
        b.textContent = "Starting\u2026";
        try {
          const res = await api("POST", "/api/experts/" + encodeURIComponent(id) + "/use", {});
          const sid = res.session_id || res.swarm_id || res.id;
          if (!sid) throw { message: "the /use endpoint returned no session id" };
          toast("Expert session started", "ok");
          // use_expert returns a RUN id (session_id = run_id), not a swarm:
          // land on the run inspector, not #/swarm/<id> (D-4).
          location.hash = "#/runs/" + encodeURIComponent(sid);
        } catch (err) {
          toast(err.message, "err");
          b.disabled = false;
          b.textContent = label;
        }
      });
    });
  } catch (e) {
    if (xIsMissing(e)) {
      body.innerHTML = emptyState("No experts backend yet", EXPERTS_MISSING);
    } else {
      body.innerHTML = errorState(e.message, true);
      const rb = $("[data-retry]", body);
      if (rb) rb.onclick = loadExperts;
    }
  }
}

renderers.experts = async function () {
  state.expertsTab = state.expertsTab || "teams";
  setView(viewHead("Experts", "teams and expert personas you can hand work to") +
    '<div class="card"><div class="tabs" role="tablist">' +
    '<button role="tab" aria-selected="true" data-xtab="teams">Teams</button>' +
    '<button role="tab" aria-selected="false" data-xtab="experts">Experts</button></div>' +
    '<div id="x-body" style="padding:16px 18px">' + loading("teams") + "</div></div>");
  const paint = (which) => {
    state.expertsTab = which;
    $$('#view [data-xtab]').forEach((b) => b.setAttribute("aria-selected", String(b.dataset.xtab === which)));
    if (which === "teams") loadTeams(); else loadExperts();
  };
  $$('#view [data-xtab]').forEach((b) => { b.onclick = () => paint(b.dataset.xtab); });
  paint(state.expertsTab);
};

(function boot() {
  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.register("sw.js").catch(() => {});
  }
  initTheme();
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
    $("#theme-toggle").onclick = toggleTheme;
    // Topbar search: jump straight into the runs view with the query.
    $("#topsearch-form").addEventListener("submit", (e) => {
      e.preventDefault();
      const q = $("#topsearch").value.trim();
      pendingSearch = q;
      if ((location.hash || "") !== "#/runs") location.hash = "#/runs";
      else renderers.runs();
      $("#topsearch").value = "";
      $("#topsearch").blur();
    });
    document.addEventListener("keydown", (e) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        $("#topsearch").focus();
        $("#topsearch").select();
      }
    });
    refreshStatus();
    setInterval(refreshStatus, 30000);
    navigate();
  }
})();
