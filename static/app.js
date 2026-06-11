/* TokenOS Control Panel */
"use strict";

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => document.querySelectorAll(sel);

const api = async (path, opts) => {
  const res = await fetch(path, opts);
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res.json();
};

const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

const fmtUSD = (v) => (v == null ? "—" : "$" + Number(v).toFixed(v < 0.01 && v > 0 ? 6 : 4));
const fmtPct = (v) => (v == null ? "—" : (Number(v) * 100).toFixed(1) + "%");
const fmtMS = (v) => (v == null ? "—" : Math.round(v) + "ms");
const fmtNum = (v) => (v == null ? "—" : Math.round(v).toLocaleString());
const fmtTime = (iso) => {
  if (!iso) return "—";
  const d = new Date(iso);
  return isNaN(d) ? "—" : d.toLocaleString();
};

const routeBadge = (route) => {
  const cls = route && route.startsWith("ESCALATE") ? "ESC" : route;
  return `<span class="badge route-${esc(cls)}">${esc(route || "—")}</span>`;
};

/* Plain-language explanations shown to newcomers in the route preview. */
const ROUTE_EXPLAIN = {
  REUSE: "A verified answer for this exact goal is already cached — it will be served for zero tokens.",
  DIRECT: "Small and unambiguous — answered with a minimal prompt on the cheapest viable provider.",
  PATCH: "A well-scoped edit — only the relevant context is sent, keeping the prompt tiny.",
  IMPLEMENT: "Real generation work — the full pipeline runs with verification of the output.",
  PARTIAL: "An interrupted task is resumed from its compressed saved state.",
  DELEGATE: "Big enough to hand to a sub-agent with a compressed delegation packet.",
  ASK: "Too ambiguous to execute safely — the cheapest action is a clarifying question.",
  ESCALATE: "Repeated failures or loops were detected — a human should take a look.",
};
const routeExplain = (route) => {
  const key = route && route.startsWith("ESCALATE") ? "ESCALATE" : route;
  return ROUTE_EXPLAIN[key] || "";
};

/* ---------- toasts ---------- */
function toast(msg, kind = "") {
  const host = $("#toasts");
  if (!host) return;
  const el = document.createElement("div");
  el.className = "toast " + kind;
  el.textContent = msg;
  host.appendChild(el);
  setTimeout(() => el.classList.add("hide"), 3600);
  setTimeout(() => el.remove(), 4000);
}

/* ---------- meta (mode badge, version) ---------- */
async function loadMeta() {
  try {
    const m = await api("/api/meta");
    const ml = $("#modeLine");
    if (ml) {
      ml.innerHTML = m.dry_run
        ? '<span class="mode-badge dry">● DRY-RUN · offline, $0</span>'
        : '<span class="mode-badge live">● LIVE · real providers</span>';
      ml.title = m.dry_run
        ? "Mock provider exercises the full pipeline offline — no API key, no spend."
        : `Live mode — ${m.providers_enabled} of ${m.providers_total} providers enabled. Executions cost real money.`;
    }
    const vt = $("#verText");
    if (vt) vt.textContent = "v" + m.version;
  } catch { /* older server without /api/meta — badge stays hidden */ }
}

/* ---------- navigation ---------- */
function switchView(view) {
  const btn = document.querySelector(`.nav-item[data-view="${view}"]`);
  if (!btn) return;
  $$(".nav-item").forEach((b) => b.classList.remove("active"));
  $$(".view").forEach((v) => v.classList.remove("active"));
  btn.classList.add("active");
  $("#view-" + view).classList.add("active");
  refreshView(view);
}

$$(".nav-item").forEach((btn) => {
  btn.addEventListener("click", () => switchView(btn.dataset.view));
});

/* Keyboard shortcuts: 1-5 switch views, Ctrl+Enter executes, Ctrl+Shift+Enter previews */
const VIEW_KEYS = { 1: "dashboard", 2: "console", 3: "tasks", 4: "executions", 5: "config" };
document.addEventListener("keydown", (ev) => {
  const inField = /^(TEXTAREA|INPUT|SELECT)$/.test(document.activeElement?.tagName || "");
  if (!inField && VIEW_KEYS[ev.key] && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
    switchView(VIEW_KEYS[ev.key]);
    return;
  }
  if ((ev.ctrlKey || ev.metaKey) && ev.key === "Enter") {
    const consoleVisible = $("#view-console").classList.contains("active");
    if (!consoleVisible) return;
    ev.preventDefault();
    (ev.shiftKey ? $("#btnRoute") : $("#btnRun")).click();
    return;
  }
  if (ev.key === "?" && !inField) { openHelp(); return; }
  if (ev.key === "Escape") closeHelp();
});

/* ---------- help modal ---------- */
function openHelp() { const m = $("#helpModal"); if (m) { m.style.display = ""; $("#helpClose")?.focus(); } }
function closeHelp() { const m = $("#helpModal"); if (m) m.style.display = "none"; }
$("#btnHelp")?.addEventListener("click", openHelp);
$("#helpClose")?.addEventListener("click", closeHelp);
$("#helpModal")?.addEventListener("click", (ev) => { if (ev.target === $("#helpModal")) closeHelp(); });

/* ---------- welcome banner (first-run onboarding) ---------- */
const WELCOME_KEY = "tokenos.welcome.dismissed";
function maybeShowWelcome(sum) {
  const b = $("#welcomeBanner");
  if (!b) return;
  let dismissed = false;
  try { dismissed = localStorage.getItem(WELCOME_KEY) === "1"; } catch {}
  const fresh = !sum || !sum.executions;
  b.style.display = fresh && !dismissed ? "" : "none";
}
$("#welcomeClose")?.addEventListener("click", () => {
  try { localStorage.setItem(WELCOME_KEY, "1"); } catch {}
  $("#welcomeBanner").style.display = "none";
});
$("#welcomeHelp")?.addEventListener("click", openHelp);
$("#welcomeTry")?.addEventListener("click", () => {
  switchView("console");
  $("#taskInput").value = "Fix the typo in the README header";
  $("#taskInput").focus();
  toast("Example loaded — click “Preview Route” to see the free routing decision.", "ok");
});

/* ---------- dashboard ---------- */
async function loadDashboard() {
  try {
    const [sum, routes, providers, bandit, drift] = await Promise.all([
      api("/api/summary"), api("/api/stats/routes"), api("/api/stats/providers"),
      api("/api/stats/bandit").catch(() => null),
      api("/api/stats/drift").catch(() => null),
    ]);
    setConn(true);
    const lu = $("#lastUpdated");
    if (lu) lu.textContent = "updated " + new Date().toLocaleTimeString();
    maybeShowWelcome(sum);

    $("#kpiGrid").innerHTML = [
      kpi("Cost / Success", fmtUSD(sum.cost_per_success), "accent"),
      kpi("Total Cost", fmtUSD(sum.total_cost_usd)),
      kpi("Success Rate", fmtPct(sum.overall_success_pct), sum.overall_success_pct >= 0.9 ? "good" : ""),
      kpi("Executions", fmtNum(sum.executions)),
      kpi("Tasks", fmtNum(sum.tasks)),
      kpi("Total Tokens", fmtNum(sum.total_tokens)),
      kpi("Avg Latency", fmtMS(sum.avg_latency_ms)),
    ].join("");

    const rt = $("#routeTable tbody");
    rt.innerHTML = (routes || []).length
      ? routes.map((r) => `<tr>
          <td>${routeBadge(r.route)}</td>
          <td>${fmtNum(r.runs)}</td>
          <td>${fmtPct(r.success_rate)}</td>
          <td>${fmtNum(r.avg_tokens_in)}</td>
          <td>${fmtNum(r.avg_tokens_out)}</td>
          <td>${fmtMS(r.avg_latency_ms)}</td>
          <td>${fmtUSD(r.cost_per_success)}</td>
        </tr>`).join("")
      : emptyRow(7, "No executions yet — run a task from the console.");

    const pt = $("#providerTable tbody");
    const provs = (providers || []).filter((p) => p.provider);
    pt.innerHTML = provs.length
      ? provs.map((p) => `<tr>
          <td>${esc(p.provider)}</td>
          <td>${fmtNum(p.runs)}</td>
          <td>${fmtPct(p.success_rate)}</td>
          <td>${fmtMS(p.avg_latency_ms)}</td>
          <td>${fmtNum(p.total_tokens)}</td>
          <td>${fmtUSD(p.total_cost_usd)}</td>
        </tr>`).join("")
      : emptyRow(6, "No provider calls yet.");

    const bt = $("#banditTable tbody");
    if (bt) {
      const arms = (bandit && bandit.arms) || [];
      bt.innerHTML = arms.length
        ? arms.map((a) => `<tr>
            <td>${esc(a.provider)}</td>
            <td>${fmtNum(a.pulls)}</td>
            <td>${a.pulls ? Number(a.mean_reward).toFixed(3) : "—"}</td>
            <td>${a.pulls ? fmtMS(a.mean_latency_ms) : "—"}</td>
            <td>${a.ucb1_score === "unexplored" ? "<span class=\"hint\">unexplored</span>" : Number(a.ucb1_score).toFixed(3)}</td>
          </tr>`).join("")
        : emptyRow(5, "No bandit arms configured.");
    }

    const dt = $("#driftTable tbody");
    if (dt) {
      const provs2 = (drift && drift.providers) || [];
      dt.innerHTML = provs2.length
        ? provs2.map((d) => `<tr>
            <td>${esc(d.provider)}</td>
            <td>${fmtNum(d.samples)}</td>
            <td>${Number(d.ratio_ewma).toFixed(3)}</td>
            <td>${d.drifting ? '<span class="badge fail">DRIFTING</span>' : '<span class="badge ok">calibrated</span>'}</td>
          </tr>`).join("")
        : emptyRow(4, "No live-usage samples yet — calibration appears after provider-billed runs.");
      const cl = $("#cacheLine");
      if (cl) {
        if (drift && drift.solution_cache) {
          const c = drift.solution_cache;
          cl.textContent = `Solution cache: ${c.entries} verified entr${c.entries === 1 ? "y" : "ies"} · ${c.zero_token_hits} zero-token hit${c.zero_token_hits === 1 ? "" : "s"}`;
        } else {
          // Clear stale telemetry after a partial refresh failure.
          cl.textContent = "";
        }
      }
    }

    const max = Math.max(1, ...(routes || []).map((r) => r.runs));
    $("#routeBars").innerHTML = (routes || []).length
      ? routes.map((r) => `<div class="bar-row">
          <div class="bar-label">${esc(r.route)}</div>
          <div class="bar-track"><div class="bar-fill" style="width:${(r.runs / max) * 100}%"></div></div>
          <div class="bar-count">${fmtNum(r.runs)}</div>
        </div>`).join("")
      : `<div class="hint">No data yet.</div>`;
  } catch (e) {
    setConn(false, e.message);
  }
}

const kpi = (label, value, cls = "") =>
  `<div class="kpi"><div class="k-label">${esc(label)}</div><div class="k-value ${cls}">${value}</div></div>`;

const emptyRow = (cols, msg) =>
  `<tr class="empty-row"><td colspan="${cols}">${esc(msg)}</td></tr>`;

function setConn(ok, msg) {
  const dot = $("#connDot"), txt = $("#connText");
  dot.className = "dot " + (ok ? "ok" : "err");
  txt.textContent = ok ? "connected" : (msg || "disconnected");
}

/* ---------- console ---------- */
$$(".chip[data-example]").forEach((chip) =>
  chip.addEventListener("click", () => {
    $("#taskInput").value = chip.dataset.example;
    $("#constraintsInput").value = "";
    $("#taskInput").focus();
    toast("Example loaded — preview the route for free, then execute.", "ok");
  }));

$("#btnClear")?.addEventListener("click", () => {
  $("#taskInput").value = "";
  $("#constraintsInput").value = "";
  $("#routePreview").style.display = "none";
  $("#runResult").style.display = "none";
  $("#taskInput").focus();
});

function constraintsList() {
  return $("#constraintsInput").value.split("\n").map((s) => s.trim()).filter(Boolean);
}

$("#btnRoute").addEventListener("click", async () => {
  const task = $("#taskInput").value.trim();
  if (!task) { toast("Enter a task first.", "err"); return; }
  const btn = $("#btnRoute");
  btn.disabled = true;
  try {
    const r = await api("/api/route", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ task }),
    });
    const d = r.decision, s = d.signals || {};
    const sigChips = Object.entries(s)
      .filter(([k, v]) => typeof v === "boolean")
      .map(([k, v]) => `<span class="sig ${v ? "on" : ""}">${esc(k)}</span>`).join("");
    const explain = routeExplain(d.route);
    $("#routePreviewBody").innerHTML = `
      ${explain ? `<div class="route-explain">${routeBadge(d.route)} ${esc(explain)}</div>` : ""}
      <dl class="kv">
        <dt>Route</dt><dd>${routeBadge(d.route)}</dd>
        <dt>Reason</dt><dd>${esc(d.reason)}</dd>
        <dt>Confidence</dt><dd>${fmtPct(s.confidence)}</dd>
        <dt>Prompt tokens (est)</dt><dd>${fmtNum(r.prompt_tokens)}</dd>
        <dt>Context tokens (est)</dt><dd>${fmtNum(r.context_tokens)}</dd>
        <dt>Provider chain</dt><dd>${(r.provider_chain || []).map(esc).join(" → ") || "—"}</dd>
      </dl>
      <div class="signals">${sigChips}</div>
      <div class="hint" style="margin-top:8px">This decision was made entirely in code — zero tokens were spent.</div>`;
    $("#routePreview").style.display = "";
  } catch (e) {
    $("#routePreviewBody").innerHTML = `<span class="badge fail">${esc(e.message)}</span>`;
    $("#routePreview").style.display = "";
    toast("Route preview failed: " + e.message, "err");
  } finally {
    btn.disabled = false;
  }
});

$("#btnRun").addEventListener("click", async () => {
  const task = $("#taskInput").value.trim();
  if (!task) { toast("Enter a task first.", "err"); return; }
  const btn = $("#btnRun");
  btn.disabled = true;
  btn.innerHTML = '<span class="spinner"></span>Executing…';
  try {
    const r = await api("/api/run", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ task, constraints: constraintsList() }),
    });
    const res = r.result || {};
    $("#runResultBody").innerHTML = `
      <dl class="kv">
        <dt>Task ID</dt><dd>${esc(res.task_id)}</dd>
        <dt>Route</dt><dd>${routeBadge(res.route)}</dd>
        <dt>Status</dt><dd>${res.success ? '<span class="badge ok">SUCCESS</span>' : '<span class="badge fail">FAILED</span>'}</dd>
        <dt>Provider / Model</dt><dd>${esc(res.provider || "—")} / ${esc(res.model || "—")}</dd>
        <dt>Tokens in / out</dt><dd>${fmtNum(res.tokens_in)} / ${fmtNum(res.tokens_out)}</dd>
        <dt>Latency</dt><dd>${fmtMS(res.latency_ms)}</dd>
        <dt>Cost</dt><dd>${fmtUSD(res.cost_usd)}</dd>
        <dt>Retries</dt><dd>${fmtNum(res.retries)}</dd>
        ${r.error ? `<dt>Error</dt><dd><span class="badge fail">${esc(r.error)}</span></dd>` : ""}
      </dl>
      <div class="output-head">
        <label class="lbl" style="margin:0">Output</label>
        <button class="link-btn" id="btnCopyOutput">copy</button>
      </div>
      <pre class="output" id="runOutput">${esc(res.output || "(empty)")}</pre>`;
    $("#runResult").style.display = "";
    $("#btnCopyOutput")?.addEventListener("click", () => {
      navigator.clipboard.writeText($("#runOutput").textContent)
        .then(() => toast("Output copied to clipboard.", "ok"))
        .catch(() => toast("Copy failed.", "err"));
    });
    toast(res.success ? "Execution succeeded." : "Execution failed — see result panel.", res.success ? "ok" : "err");
  } catch (e) {
    $("#runResultBody").innerHTML = `<span class="badge fail">${esc(e.message)}</span>`;
    $("#runResult").style.display = "";
    toast("Execution error: " + e.message, "err");
  } finally {
    btn.disabled = false; btn.textContent = "Execute";
  }
});

/* ---------- tasks ---------- */
let tasksCache = [];
function renderTasks() {
  const q = ($("#taskFilter")?.value || "").trim().toLowerCase();
  const rows = q
    ? tasksCache.filter((t) =>
        (t.task_id + " " + t.goal + " " + t.status).toLowerCase().includes(q))
    : tasksCache;
  const tb = $("#tasksTable tbody");
  tb.innerHTML = rows.length
    ? rows.map((t) => `<tr>
        <td>${esc(t.task_id)}</td>
        <td class="goal-cell">${esc(t.goal)}</td>
        <td><span class="badge status status-${esc(t.status)}">${esc(t.status)}</span></td>
        <td>${t.blocked ? "⚠" : ""}</td>
        <td>${fmtTime(t.updated_at)}</td>
        <td><button class="link-btn" data-trace="${esc(t.task_id)}">view</button></td>
      </tr>`).join("")
    : emptyRow(6, tasksCache.length ? "No tasks match the filter." : "No tasks yet — run one from the console.");
  tb.querySelectorAll("[data-trace]").forEach((b) =>
    b.addEventListener("click", () => loadTrace(b.dataset.trace)));
}
$("#taskFilter")?.addEventListener("input", renderTasks);

async function loadTasks() {
  try {
    tasksCache = (await api("/api/tasks")) || [];
    renderTasks();
  } catch (e) { setConn(false, e.message); }
}

async function loadTrace(taskID) {
  try {
    const events = await api("/api/traces/" + encodeURIComponent(taskID));
    $("#traceTaskId").textContent = taskID;
    $("#traceBody").innerHTML = (events || []).length
      ? events.map((ev) => `<div class="trace-event">
          <div class="trace-time">${new Date(ev.ts).toLocaleTimeString()}</div>
          <div class="trace-kind ${esc(ev.kind)}">${esc(ev.kind)}</div>
          <div class="trace-summary">${esc(ev.summary || "")}</div>
        </div>`).join("")
      : `<div class="hint">No flight-recorder events for this task.</div>`;
    $("#tracePanel").style.display = "";
    $("#tracePanel").scrollIntoView({ behavior: "smooth" });
  } catch (e) { setConn(false, e.message); }
}

/* ---------- executions ---------- */
let execCache = [];
function renderExecutions() {
  const q = ($("#execFilter")?.value || "").trim().toLowerCase();
  const st = $("#execStatusFilter")?.value || "";
  const rows = execCache.filter((e) => {
    if (st === "ok" && !e.success) return false;
    if (st === "fail" && e.success) return false;
    if (q && !((e.task_id + " " + e.route + " " + (e.provider || "")).toLowerCase().includes(q))) return false;
    return true;
  });
  const tb = $("#execTable tbody");
  tb.innerHTML = rows.length
    ? rows.map((e) => `<tr>
        <td>${e.id}</td>
        <td>${esc(e.task_id)}</td>
        <td>${routeBadge(e.route)}</td>
        <td>${esc(e.provider || "—")}</td>
        <td>${fmtNum(e.tokens_in)}</td>
        <td>${fmtNum(e.tokens_out)}</td>
        <td>${fmtMS(e.latency_ms)}</td>
        <td>${fmtNum(e.retries)}</td>
        <td>${fmtUSD(e.est_cost_usd)}</td>
        <td>${e.success ? '<span class="badge ok">✓</span>' : '<span class="badge fail">✗</span>'}</td>
      </tr>`).join("")
    : emptyRow(10, execCache.length ? "No executions match the filter." : "No executions recorded — run a task from the console.");
}
$("#execFilter")?.addEventListener("input", renderExecutions);
$("#execStatusFilter")?.addEventListener("change", renderExecutions);

async function loadExecutions() {
  try {
    execCache = (await api("/api/executions")) || [];
    renderExecutions();
  } catch (e) { setConn(false, e.message); }
}

/* ---------- config ---------- */
async function loadConfig() {
  try {
    const cfg = await api("/api/config");
    const provs = Object.entries(cfg.providers || {}).map(([name, p]) => `
      <div class="panel cfg-section">
        <h3>${esc(name)} ${p.disabled ? '<span class="badge status">disabled</span>' : '<span class="badge ok">enabled</span>'}</h3>
        <dl class="kv">
          <dt>Adapter</dt><dd>${esc(p.adapter)}</dd>
          <dt>Model</dt><dd>${esc(p.model || "—")}</dd>
          <dt>Priority</dt><dd>${p.priority}</dd>
          <dt>Max context</dt><dd>${fmtNum(p.max_context_tokens)}</dd>
          <dt>Cost in/out ($/Mtok)</dt><dd>${p.cost_per_mtok_in ?? 0} / ${p.cost_per_mtok_out ?? 0}</dd>
          <dt>Auth</dt><dd>${esc(p.auth_type)}${p.api_key_env ? ` (<code class="inline">${esc(p.api_key_env)}</code>)` : ""}</dd>
          <dt>Include</dt><dd>${(p.models?.include || []).map(esc).join(", ") || "all"}</dd>
          <dt>Exclude</dt><dd>${(p.models?.exclude || []).map(esc).join(", ") || "none"}</dd>
        </dl>
      </div>`).join("");

    const rules = (cfg.execution_routing || []).map((r) => `
      <div class="trace-event">
        <div class="trace-kind prompt">${(r.route_types || []).map(esc).join(", ")}</div>
        <div class="trace-summary">→ <b>${esc(r.provider)}</b>${r.fallback ? ` (fallback: ${esc(r.fallback)})` : ""} · timeout ${fmtNum(r.timeout_ms)}ms</div>
        <div></div>
      </div>`).join("");

    const pol = cfg.policy || {};
    $("#configBody").innerHTML = `
      <div class="panel cfg-section">
        <h3>Router Policy</h3>
        <dl class="kv">
          <dt>ASK threshold</dt><dd>${pol.ask_threshold}</dd>
          <dt>DIRECT max tokens</dt><dd>${fmtNum(pol.direct_max_tokens)}</dd>
          <dt>Delegation penalty</dt><dd>${fmtNum(pol.delegation_penalty)}</dd>
          <dt>Delegation min scale</dt><dd>${pol.delegation_min_scale}</dd>
          <dt>Pricing α / β</dt><dd>${cfg.pricing?.alpha} / ${cfg.pricing?.beta}</dd>
        </dl>
      </div>
      <div class="panel cfg-section"><h3>Routing Rules</h3>${rules || '<div class="hint">none</div>'}</div>
      ${provs}`;
  } catch (e) { setConn(false, e.message); }
}

/* ---------- refresh loop ---------- */
function refreshView(view) {
  if (view === "dashboard") loadDashboard();
  else if (view === "tasks") loadTasks();
  else if (view === "executions") loadExecutions();
  else if (view === "config") loadConfig();
}

loadMeta();
loadDashboard();
setInterval(() => {
  if (document.hidden) return; // skip refresh when tab is in the background
  const active = document.querySelector(".nav-item.active")?.dataset.view;
  if (active === "dashboard") loadDashboard();
  else if (active === "executions") loadExecutions();
}, 5000);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refreshView(document.querySelector(".nav-item.active")?.dataset.view || "dashboard");
});
