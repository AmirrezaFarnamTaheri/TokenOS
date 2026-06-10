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

/* ---------- navigation ---------- */
$$(".nav-item").forEach((btn) => {
  btn.addEventListener("click", () => {
    $$(".nav-item").forEach((b) => b.classList.remove("active"));
    $$(".view").forEach((v) => v.classList.remove("active"));
    btn.classList.add("active");
    $("#view-" + btn.dataset.view).classList.add("active");
    refreshView(btn.dataset.view);
  });
});

/* ---------- dashboard ---------- */
async function loadDashboard() {
  try {
    const [sum, routes, providers] = await Promise.all([
      api("/api/summary"), api("/api/stats/routes"), api("/api/stats/providers"),
    ]);
    setConn(true);

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
function constraintsList() {
  return $("#constraintsInput").value.split("\n").map((s) => s.trim()).filter(Boolean);
}

$("#btnRoute").addEventListener("click", async () => {
  const task = $("#taskInput").value.trim();
  if (!task) return;
  try {
    const r = await api("/api/route", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ task }),
    });
    const d = r.decision, s = d.signals || {};
    const sigChips = Object.entries(s)
      .filter(([k, v]) => typeof v === "boolean")
      .map(([k, v]) => `<span class="sig ${v ? "on" : ""}">${esc(k)}</span>`).join("");
    $("#routePreviewBody").innerHTML = `
      <dl class="kv">
        <dt>Route</dt><dd>${routeBadge(d.route)}</dd>
        <dt>Reason</dt><dd>${esc(d.reason)}</dd>
        <dt>Confidence</dt><dd>${fmtPct(s.confidence)}</dd>
        <dt>Prompt tokens (est)</dt><dd>${fmtNum(r.prompt_tokens)}</dd>
        <dt>Context tokens (est)</dt><dd>${fmtNum(r.context_tokens)}</dd>
        <dt>Provider chain</dt><dd>${(r.provider_chain || []).map(esc).join(" → ") || "—"}</dd>
      </dl>
      <div class="signals">${sigChips}</div>`;
    $("#routePreview").style.display = "";
  } catch (e) {
    $("#routePreviewBody").innerHTML = `<span class="badge fail">${esc(e.message)}</span>`;
    $("#routePreview").style.display = "";
  }
});

$("#btnRun").addEventListener("click", async () => {
  const task = $("#taskInput").value.trim();
  if (!task) return;
  const btn = $("#btnRun");
  btn.disabled = true; btn.textContent = "Executing…";
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
      <label class="lbl">Output</label>
      <pre class="output">${esc(res.output || "(empty)")}</pre>`;
    $("#runResult").style.display = "";
  } catch (e) {
    $("#runResultBody").innerHTML = `<span class="badge fail">${esc(e.message)}</span>`;
    $("#runResult").style.display = "";
  } finally {
    btn.disabled = false; btn.textContent = "Execute";
  }
});

/* ---------- tasks ---------- */
async function loadTasks() {
  try {
    const tasks = await api("/api/tasks");
    const tb = $("#tasksTable tbody");
    tb.innerHTML = (tasks || []).length
      ? tasks.map((t) => `<tr>
          <td>${esc(t.task_id)}</td>
          <td class="goal-cell">${esc(t.goal)}</td>
          <td><span class="badge status status-${esc(t.status)}">${esc(t.status)}</span></td>
          <td>${t.blocked ? "⚠" : ""}</td>
          <td>${fmtTime(t.updated_at)}</td>
          <td><button class="link-btn" data-trace="${esc(t.task_id)}">view</button></td>
        </tr>`).join("")
      : emptyRow(6, "No tasks yet.");
    tb.querySelectorAll("[data-trace]").forEach((b) =>
      b.addEventListener("click", () => loadTrace(b.dataset.trace)));
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
async function loadExecutions() {
  try {
    const execs = await api("/api/executions");
    const tb = $("#execTable tbody");
    tb.innerHTML = (execs || []).length
      ? execs.map((e) => `<tr>
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
      : emptyRow(10, "No executions recorded.");
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

loadDashboard();
setInterval(() => {
  const active = document.querySelector(".nav-item.active")?.dataset.view;
  if (active === "dashboard") loadDashboard();
  else if (active === "executions") loadExecutions();
}, 5000);
