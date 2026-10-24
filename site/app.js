// The dorfq simulator report: the comparison table with headlines against
// the `github-like` column, and a replay of each policy's first
// replication. Reads the documents `dorfq-sim run --json` writes.
"use strict";

const SCHEMA = 1;
const BASELINE = "github-like";
const DEFAULT_REPORT = "busy";

// Fixed text shown beside a named report's world.
const REPORT_NOTES = {
  busy:
    "bisect's max_batch was chosen by measurement in this world, and its best " +
    "measured cap is 1: its column is the naive queue on all six runners, so " +
    "its deltas against github-like show the sixth runner, not pooling.",
};

// Metric rows, as the command line's table prints them: key and decimals.
const ROWS = [
  ["arrived", 1], ["landed", 1], ["ejected_bad", 1], ["ejected_good", 1],
  ["withdrawn", 1], ["blocked", 1], ["unfinished", 1], ["runs", 1],
  ["mean_batch_size", 2], ["runner_hours", 1], ["runner_hours_per_landed", 2],
  ["wasted_fraction", 3], ["land_p50_hours", 2], ["land_p95_hours", 2],
  ["reject_p50_hours", 2], ["reject_p95_hours", 2], ["utilisation", 3],
];

// Headline deltas against the baseline; lower is better for every one.
const HEADLINES = [
  ["runner_hours_per_landed", "runner-hours per landed pull request", 2, ""],
  ["land_p50_hours", "median time to land", 2, " h"],
  ["land_p95_hours", "95th-percentile time to land", 2, " h"],
  ["ejected_good", "good pull requests ejected", 1, ""],
  ["blocked", "pull requests blocked by their base leaving", 1, ""],
];

const $ = (id) => document.getElementById(id);

function setStatus(text, error) {
  const el = $("status");
  el.textContent = text || "";
  el.classList.toggle("error", Boolean(error));
}

async function fetchJson(path) {
  const response = await fetch(path);
  if (!response.ok) throw new Error(`${path}: HTTP ${response.status}`);
  return response.json();
}

async function boot() {
  $("file-input").addEventListener("change", onFile);
  let names;
  try {
    names = await fetchJson("reports/index.json");
  } catch (error) {
    offerPicker(`Could not load the report list (${error.message}). Open a report document instead.`);
    return;
  }
  if (!Array.isArray(names) || names.length === 0) {
    setStatus("reports/index.json lists no reports.", true);
    return;
  }
  const select = $("report-select");
  for (const name of names) select.add(new Option(name, name));
  const asked = new URLSearchParams(location.search).get("report");
  if (asked !== null && !names.includes(asked)) {
    setStatus(`No report named "${asked}"; the reports are ${names.join(", ")}.`, true);
    return;
  }
  const chosen = asked ?? (names.includes(DEFAULT_REPORT) ? DEFAULT_REPORT : names[0]);
  select.value = chosen;
  select.addEventListener("change", () => {
    const url = new URL(location.href);
    url.searchParams.set("report", select.value);
    history.replaceState(null, "", url);
    loadNamed(select.value);
  });
  loadNamed(chosen);
}

function offerPicker(message) {
  setStatus(message, false);
  $("report-select").hidden = true;
  $("file-label").hidden = false;
}

async function loadNamed(name) {
  setStatus(`Loading ${name}…`);
  let doc;
  try {
    doc = await fetchJson(`reports/${encodeURIComponent(name)}.json`);
  } catch (error) {
    offerPicker(`Could not load report "${name}" (${error.message}). Open a report document instead.`);
    return;
  }
  show(doc);
}

function onFile(event) {
  const file = event.target.files[0];
  if (!file) return;
  file.text().then(
    (text) => {
      let doc;
      try {
        doc = JSON.parse(text);
      } catch (error) {
        setStatus(`${file.name} is not JSON: ${error.message}`, true);
        return;
      }
      show(doc);
    },
    (error) => setStatus(`Could not read ${file.name}: ${error.message}`, true),
  );
}

function show(doc) {
  if (doc === null || typeof doc !== "object" || doc.schema !== SCHEMA) {
    const said = doc && typeof doc === "object" ? doc.schema : undefined;
    setStatus(`This page reads report schema ${SCHEMA}; the document's schema is ${JSON.stringify(said)}. Not rendered.`, true);
    $("comparison").hidden = true;
    $("replay").hidden = true;
    return;
  }
  setStatus("");
  try {
    renderComparison(doc);
    replay.load(doc);
  } catch (error) {
    console.error(error);
    setStatus(`The report was loaded but could not be rendered: ${error.message}`, true);
    $("comparison").hidden = true;
    $("replay").hidden = true;
  }
}

function policyConfig(doc, name) {
  return doc.config.policy.find((p) => p.name === name) || { name };
}

function describeConfig(cfg) {
  const parts = [cfg.kind];
  if (cfg.lanes !== undefined) parts.push(`lanes ${cfg.lanes}`);
  if (cfg.max_batch !== undefined) parts.push(`max_batch ${cfg.max_batch}`);
  return parts.join(" · ");
}

// Mean, min and max of a metric over the replications that ran and have it.
function stat(policy, key) {
  const values = policy.replications
    .filter((r) => r.metrics && r.metrics[key] !== null && r.metrics[key] !== undefined)
    .map((r) => r.metrics[key]);
  if (values.length === 0) return null;
  return {
    mean: values.reduce((a, b) => a + b, 0) / values.length,
    min: Math.min(...values),
    max: Math.max(...values),
  };
}

function percent(p) {
  return `${Number((p * 100).toPrecision(3))}%`;
}

function describeWorld(doc) {
  const c = doc.config;
  const w = c.world;
  return (
    `${w.arrivals_per_hour} pull requests an hour for ${w.hours} hours, then up to ` +
    `${w.drain_hours} hours to drain, on ${w.runners} CI runners; a run takes ` +
    `${w.ci_minutes.min}–${w.ci_minutes.max} minutes. ${percent(w.p_bad)} of pull requests ` +
    `are bad on their own, ${percent(w.p_interact)} of pairs whose queue time overlaps break ` +
    `only together, ${percent(w.p_conflict)} arrive conflicting with trunk, and ` +
    `${percent(w.p_flake)} of runs fail for no reason. A queued pull request is re-pushed ` +
    `${w.repush_per_hour} times an hour and withdrawn ${w.withdraw_per_hour} times an hour. ` +
    `${percent(w.p_stacked)} of arrivals are cut from a recent branch and stack on it only ` +
    `while it is still queued — so a faster policy sees fewer stacks — and ` +
    `${percent(w.p_atomic)} of those that stack land only with their ancestors. ` +
    `${c.replications} replications from seed ${c.seed}; every policy sees the same ` +
    `arrivals and the same hidden truths.`
  );
}

function renderComparison(doc) {
  $("comparison").hidden = false;
  $("report-name").textContent = doc.name;
  $("world").textContent = describeWorld(doc);
  const note = REPORT_NOTES[doc.name];
  $("report-note").hidden = !note;
  $("report-note").textContent = note || "";

  const headlines = $("headlines");
  headlines.replaceChildren();
  const baseline = doc.policies.find((p) => p.name === BASELINE);
  if (baseline) {
    headlines.append(baselineCard(doc, baseline));
    for (const policy of doc.policies) {
      if (policy !== baseline) headlines.append(deltaCard(doc, policy, baseline));
    }
  }
  renderTable(doc, baseline);
}

function card(doc, policy, extra) {
  const el = document.createElement("div");
  el.className = `card ${extra}`;
  const h = document.createElement("h3");
  h.textContent = policy.name;
  const cfg = document.createElement("div");
  cfg.className = "cfg";
  cfg.textContent = describeConfig(policyConfig(doc, policy.name));
  el.append(h, cfg);
  return el;
}

function deltaRow(what, num, cls, from) {
  const row = document.createElement("div");
  row.className = "delta";
  const w = document.createElement("span");
  w.className = "what";
  w.textContent = what;
  const n = document.createElement("span");
  n.className = `num ${cls}`;
  n.textContent = num;
  row.append(w, n);
  if (from) {
    const f = document.createElement("span");
    f.className = "from";
    f.textContent = from;
    row.append(f);
  }
  return row;
}

function baselineCard(doc, baseline) {
  const el = card(doc, baseline, "baseline-card");
  el.querySelector(".cfg").textContent += " — the baseline: GitHub's merge queue, approximately";
  for (const [key, what, decimals, unit] of HEADLINES) {
    const s = stat(baseline, key);
    el.append(deltaRow(what, s ? `${s.mean.toFixed(decimals)}${unit}` : "-", "same"));
  }
  return el;
}

function deltaCard(doc, policy, baseline) {
  const el = card(doc, policy, "");
  for (const [key, what, decimals, unit] of HEADLINES) {
    const v = stat(policy, key);
    const b = stat(baseline, key);
    if (!v || !b) {
      el.append(deltaRow(what, "-", "same"));
      continue;
    }
    const diff = v.mean - b.mean;
    const epsilon = 0.5 * 10 ** -decimals;
    const cls = diff < -epsilon ? "better" : diff > epsilon ? "worse" : "same";
    const arrow = cls === "better" ? "▼ " : cls === "worse" ? "▲ " : "";
    const sign = diff > 0 ? "+" : diff < 0 ? "−" : "±";
    let num = `${arrow}${sign}${Math.abs(diff).toFixed(decimals)}${unit}`;
    if (b.mean !== 0) num += ` (${sign}${Math.abs((100 * diff) / b.mean).toFixed(0)}%)`;
    el.append(deltaRow(what, num, cls,
      `${v.mean.toFixed(decimals)}${unit} against ${b.mean.toFixed(decimals)}${unit}`));
  }
  return el;
}

function renderTable(doc, baseline) {
  const table = $("metrics");
  table.replaceChildren();
  const head = table.createTHead().insertRow();
  head.append(Object.assign(document.createElement("th"), { textContent: "metric" }));
  for (const policy of doc.policies) {
    const th = document.createElement("th");
    if (policy === baseline) th.className = "base";
    th.textContent = policy.name;
    const small = document.createElement("small");
    small.textContent = describeConfig(policyConfig(doc, policy.name));
    th.append(small);
    head.append(th);
  }
  const body = table.createTBody();
  for (const [key, decimals] of ROWS) {
    const row = body.insertRow();
    row.insertCell().textContent = key;
    for (const policy of doc.policies) {
      const cell = row.insertCell();
      if (policy === baseline) cell.className = "base";
      const s = stat(policy, key);
      cell.textContent = s
        ? `${s.mean.toFixed(decimals)} [${s.min.toFixed(decimals)}–${s.max.toFixed(decimals)}]`
        : "-";
    }
  }
  const row = body.insertRow();
  row.insertCell().textContent = "failure";
  for (const policy of doc.policies) {
    const cell = row.insertCell();
    const errors = policy.replications.filter((r) => r.error !== undefined);
    if (errors.length === 0) {
      cell.textContent = "-";
    } else {
      cell.className = "error";
      cell.textContent = `${errors.length}: ${errors[0].error}`;
    }
  }
}

function clockText(t) {
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  return `${h}h${String(m).padStart(2, "0")}`;
}

function list(xs) {
  return `[${xs.join(", ")}]`;
}

// The text line of an event, in the spirit of `dorfq-sim run --trace`.
function eventText(e) {
  switch (e.kind) {
    case "arrive": {
      let s = `arrive ${e.pr}`;
      if (e.base !== null) s += ` on ${e.base}`;
      if (e.atomic) s += " atomic";
      const truth = [e.truth && e.truth.bad && "bad", e.truth && e.truth.conflicts && "conflicts"].filter(Boolean);
      if (truth.length) s += ` (${truth.join(", ")})`;
      return s;
    }
    case "repush": return `repush ${e.pr}`;
    case "withdraw": return `withdraw ${e.pr}`;
    case "blocked": return `blocked ${e.pr} on ${e.on}`;
    case "start": return `start ${e.batch} ${list(e.members)} on ${e.base === null ? "trunk" : e.base}`;
    case "run_begin": return `ci ${e.batch} begins`;
    case "run_end":
      return `ci ${e.batch} ${e.passed ? "passed" : "failed"}${e.broken ? " broken" : ""}${e.flake ? " flake" : ""}`;
    case "land": return `land ${e.batch} ${list(e.members)}`;
    case "refused": return `refused ${e.batch}: ${e.reason}`;
    case "eject": return `eject ${e.pr} ${e.justified ? "justified" : "unjustified"}: ${e.reason}`;
    case "retire": return `retire ${e.batch}${e.cancelled ? " cancelled" : ""}`;
    case "step": {
      let s = `queued=${e.queued} in_flight=${e.in_flight}`;
      if (e.land.length) s += ` | land ${list(e.land)}`;
      if (e.start.length) s += ` | start ${e.start.map((x) => `${x.batch}${list(x.members)}`).join(" ")}`;
      if (e.eject.length) s += ` | eject ${e.eject.map((x) => `${x.pr} (${x.reason})`).join(" ")}`;
      if (e.retire.length) s += ` | retire ${list(e.retire)}`;
      return s;
    }
    default: return e.kind;
  }
}

// One policy's trace folded into the world as it stood at a time.
class World {
  constructor(events, runners) {
    this.events = events;
    this.runners = runners;
    this.reset();
  }

  reset() {
    this.index = 0;
    this.now = 0;
    this.prs = new Map();
    this.queued = new Set();
    this.batches = new Map();
    this.waiting = [];
    this.slots = new Array(this.runners).fill(null);
    this.runs = [];
    this.trunk = [];
    this.departed = [];
    this.count = { landed: 0, ejected_bad: 0, ejected_good: 0, withdrawn: 0, blocked: 0, runs: 0 };
  }

  advanceTo(t) {
    if (t < this.now) this.reset();
    while (this.index < this.events.length && this.events[this.index].t <= t) {
      this.apply(this.events[this.index]);
      this.index += 1;
    }
    this.now = t;
  }

  depart(number, how, t, detail) {
    const pr = this.prs.get(number);
    if (!pr) return;
    pr.state = how;
    pr.left = t;
    Object.assign(pr, detail);
    this.queued.delete(number);
    this.departed.push(pr);
  }

  freeSlot(batch) {
    const i = this.slots.indexOf(batch.id);
    if (i >= 0) this.slots[i] = null;
  }

  apply(e) {
    switch (e.kind) {
      case "arrive":
        this.prs.set(e.pr, {
          pr: e.pr, base: e.base, atomic: e.atomic, arrived: e.t, state: "queued",
          bad: Boolean(e.truth && e.truth.bad), conflicts: Boolean(e.truth && e.truth.conflicts),
          batches: new Set(), repushed: null,
        });
        this.queued.add(e.pr);
        break;
      case "repush": {
        const pr = this.prs.get(e.pr);
        if (pr) pr.repushed = e.t;
        break;
      }
      case "withdraw":
        this.count.withdrawn += 1;
        this.depart(e.pr, "withdrawn", e.t, {});
        break;
      case "blocked":
        this.count.blocked += 1;
        this.depart(e.pr, "blocked", e.t, { on: e.on });
        break;
      case "start": {
        const batch = {
          id: e.batch, base: e.base, members: e.members, started: e.t,
          began: null, ended: null, runner: null, verdict: null, landed: false,
        };
        this.batches.set(e.batch, batch);
        this.waiting.push(batch);
        for (const m of e.members) {
          const pr = this.prs.get(m);
          if (pr) pr.batches.add(e.batch);
        }
        break;
      }
      case "run_begin": {
        const batch = this.batches.get(e.batch);
        if (!batch) break;
        this.waiting = this.waiting.filter((b) => b !== batch);
        batch.began = e.t;
        let slot = this.slots.indexOf(null);
        if (slot < 0) {
          slot = this.slots.length;
          this.slots.push(null);
        }
        this.slots[slot] = batch.id;
        batch.runner = slot;
        this.runs.push(batch);
        this.count.runs += 1;
        break;
      }
      case "run_end": {
        const batch = this.batches.get(e.batch);
        if (!batch) break;
        batch.ended = e.t;
        batch.verdict = e.passed ? "passed" : e.flake ? "flake" : "failed";
        batch.broken = e.broken;
        this.freeSlot(batch);
        break;
      }
      case "land": {
        const batch = this.batches.get(e.batch);
        if (batch) batch.landed = true;
        this.trunk.push({ batch: e.batch, members: e.members, t: e.t });
        const landed = new Set(e.members);
        for (const m of e.members) {
          this.count.landed += 1;
          this.depart(m, "landed", e.t, { batch: e.batch });
        }
        for (const q of this.queued) {
          const pr = this.prs.get(q);
          if (pr.base !== null && landed.has(pr.base)) pr.base = null;
        }
        break;
      }
      case "refused": {
        const batch = this.batches.get(e.batch);
        if (batch) batch.refused = e.reason;
        break;
      }
      case "eject": {
        if (e.justified) this.count.ejected_bad += 1;
        else this.count.ejected_good += 1;
        this.depart(e.pr, "ejected", e.t, { reason: e.reason, justified: e.justified });
        break;
      }
      case "retire": {
        const batch = this.batches.get(e.batch);
        if (!batch) break;
        batch.retired = e.t;
        if (batch.began === null) {
          this.waiting = this.waiting.filter((b) => b !== batch);
        } else if (batch.ended === null) {
          batch.ended = e.t;
          batch.verdict = "cancelled";
          this.freeSlot(batch);
        }
        for (const m of batch.members) {
          const pr = this.prs.get(m);
          if (pr) pr.batches.delete(batch.id);
        }
        break;
      }
      default:
        break;
    }
  }
}

// ---------------------------------------------------------------- drawing

const COLOURS = {
  bg: "#12161e", line: "#2a3140", text: "#dfe4ec", muted: "#8a94a6", faint: "#4d5568",
  accent: "#6aa7ff", good: "#3ecf8e", bad: "#ff6b6b", warn: "#f5b041", flake: "#c792ea",
  cancel: "#5c6578", chip: "#2b3242",
};

const VERDICT = { passed: COLOURS.good, failed: COLOURS.bad, flake: COLOURS.flake, cancelled: COLOURS.cancel };
const DEPARTURE = { landed: COLOURS.good, ejected: COLOURS.bad, withdrawn: COLOURS.cancel, blocked: COLOURS.warn };

const WINDOW = 6 * 3600;
const LEFT = 92;
const ROW = 26;
const CHIP_W = 46;
const CHIP_H = 22;
const GAP = 6;
const DEPARTED_FOR = 3 * 3600;

function roundRect(ctx, x, y, w, h, r) {
  ctx.beginPath();
  ctx.roundRect(x, y, w, h, r);
}

function fitText(ctx, text, width) {
  if (ctx.measureText(text).width <= width) return text;
  let s = text;
  while (s.length > 1 && ctx.measureText(`${s}…`).width > width) s = s.slice(0, -1);
  return s.length > 1 ? `${s}…` : "";
}

// The queued pull requests as trees: each root (based on trunk) followed by
// its descendants depth-first, children in number order.
function forest(world) {
  const children = new Map();
  const roots = [];
  for (const n of [...world.queued].sort((a, b) => a - b)) {
    const pr = world.prs.get(n);
    if (pr.base !== null && world.queued.has(pr.base)) {
      if (!children.has(pr.base)) children.set(pr.base, []);
      children.get(pr.base).push(n);
    } else {
      roots.push(n);
    }
  }
  const trees = [];
  for (const root of roots) {
    const tree = [];
    const walk = (n, depth) => {
      tree.push({ n, depth });
      for (const c of children.get(n) || []) walk(c, depth + 1);
    };
    walk(root, 0);
    trees.push(tree);
  }
  return trees;
}

function draw(canvas, world, hits) {
  const ctx = canvas.getContext("2d");
  const dpr = window.devicePixelRatio || 1;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  if (canvas.width !== Math.round(width * dpr) || canvas.height !== Math.round(height * dpr)) {
    canvas.width = Math.round(width * dpr);
    canvas.height = Math.round(height * dpr);
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.fillStyle = COLOURS.bg;
  ctx.fillRect(0, 0, width, height);
  ctx.font = "12px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
  ctx.textBaseline = "middle";
  hits.length = 0;

  const now = world.now;
  let y = drawRunners(ctx, world, hits, now, width);
  y = drawQueue(ctx, world, hits, now, width, y + 14, height - 150);
  drawDeparted(ctx, world, hits, now, width, height - 128);
  drawTrunk(ctx, world, hits, now, width, height - 56);
}

function sectionLabel(ctx, text, x, y) {
  ctx.fillStyle = COLOURS.muted;
  ctx.font = "11px system-ui, sans-serif";
  ctx.fillText(text.toUpperCase(), x, y);
  ctx.font = "12px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
}

function drawRunners(ctx, world, hits, now, width) {
  const top = 26;
  const right = width - 12;
  const x = (t) => LEFT + ((t - (now - WINDOW)) / WINDOW) * (right - LEFT);
  sectionLabel(ctx, "runners", 12, 12);

  ctx.strokeStyle = COLOURS.line;
  ctx.fillStyle = COLOURS.faint;
  ctx.lineWidth = 1;
  for (let h = Math.ceil((now - WINDOW) / 3600); h * 3600 <= now; h += 1) {
    const hx = x(h * 3600);
    if (hx < LEFT) continue;
    ctx.beginPath();
    ctx.moveTo(hx, top - 4);
    ctx.lineTo(hx, top + world.slots.length * ROW);
    ctx.stroke();
    ctx.fillText(`${h}h`, hx + 3, top - 10);
  }

  for (let i = 0; i < world.slots.length; i += 1) {
    const ry = top + i * ROW;
    ctx.fillStyle = COLOURS.muted;
    ctx.fillText(`runner ${i + 1}`, 12, ry + ROW / 2);
    ctx.strokeStyle = COLOURS.line;
    ctx.beginPath();
    ctx.moveTo(LEFT, ry + ROW);
    ctx.lineTo(right, ry + ROW);
    ctx.stroke();
  }

  ctx.save();
  ctx.beginPath();
  ctx.rect(LEFT, top, right - LEFT, world.slots.length * ROW);
  ctx.clip();
  for (let i = world.runs.length - 1; i >= 0; i -= 1) {
    const b = world.runs[i];
    const end = b.ended === null ? now : b.ended;
    if (end < now - WINDOW) {
      if (b.began < now - 2 * WINDOW) break;
      continue;
    }
    const bx = Math.max(x(b.began), LEFT - 4);
    const bw = Math.max(x(end) - bx, 2);
    const by = top + b.runner * ROW + 3;
    const running = b.ended === null;
    ctx.globalAlpha = running ? 1 : b.verdict === "passed" && !b.landed ? 0.5 : 0.85;
    ctx.fillStyle = running ? COLOURS.accent : VERDICT[b.verdict];
    roundRect(ctx, bx, by, bw, ROW - 6, 4);
    ctx.fill();
    if (b.broken && !running) {
      ctx.strokeStyle = COLOURS.text;
      ctx.lineWidth = 1.5;
      ctx.stroke();
      ctx.lineWidth = 1;
    }
    ctx.globalAlpha = 1;
    ctx.fillStyle = "#08101e";
    const label = fitText(ctx, `${b.id} ${list(b.members)}`, bw - 8);
    if (label) ctx.fillText(label, bx + 4, by + (ROW - 6) / 2);
    hits.push({ x: bx, y: by, w: bw, h: ROW - 6, text: () => batchText(world, b) });
  }
  ctx.restore();

  ctx.strokeStyle = COLOURS.text;
  ctx.globalAlpha = 0.4;
  ctx.beginPath();
  ctx.moveTo(right, top - 4);
  ctx.lineTo(right, top + world.slots.length * ROW);
  ctx.stroke();
  ctx.globalAlpha = 1;

  const wy = top + world.slots.length * ROW + 6;
  ctx.fillStyle = COLOURS.muted;
  ctx.fillText("waiting", 12, wy + ROW / 2);
  let wx = LEFT;
  for (const b of world.waiting) {
    const label = `${b.id} ${list(b.members)}`;
    const w = Math.min(ctx.measureText(label).width + 12, 220);
    if (wx + w > right) {
      ctx.fillStyle = COLOURS.muted;
      ctx.fillText(`+${world.waiting.length - world.waiting.indexOf(b)}`, wx, wy + ROW / 2);
      break;
    }
    ctx.setLineDash([4, 3]);
    ctx.strokeStyle = COLOURS.accent;
    roundRect(ctx, wx, wy + 3, w, ROW - 6, 4);
    ctx.stroke();
    ctx.setLineDash([]);
    ctx.fillStyle = COLOURS.text;
    ctx.fillText(fitText(ctx, label, w - 10), wx + 6, wy + ROW / 2);
    hits.push({ x: wx, y: wy + 3, w, h: ROW - 6, text: () => batchText(world, b) });
    wx += w + GAP;
  }
  return wy + ROW;
}

function drawChip(ctx, pr, x, y, now, faded) {
  const inBatch = pr.batches.size > 0;
  const age = now - pr.arrived;
  const pop = pr.state === "queued" && age < 1200 ? 1 + 0.25 * (1 - age / 1200) : 1;
  const w = CHIP_W * pop;
  const h = CHIP_H * pop;
  const cx = x - (w - CHIP_W) / 2;
  const cy = y - (h - CHIP_H) / 2;
  ctx.globalAlpha = faded;
  ctx.fillStyle = pr.state === "queued" ? (inBatch ? "#22385a" : COLOURS.chip) : DEPARTURE[pr.state];
  roundRect(ctx, cx, cy, w, h, 5);
  ctx.fill();
  if (pr.state === "queued" && (inBatch || pr.atomic)) {
    ctx.strokeStyle = pr.atomic ? COLOURS.warn : COLOURS.accent;
    ctx.lineWidth = 2;
    if (pr.atomic && inBatch) ctx.setLineDash([4, 2]);
    ctx.stroke();
    ctx.setLineDash([]);
    ctx.lineWidth = 1;
  }
  if (pr.state === "ejected" && !pr.justified) {
    ctx.strokeStyle = COLOURS.text;
    ctx.lineWidth = 2;
    ctx.stroke();
    ctx.lineWidth = 1;
  }
  if (pr.repushed !== null && now - pr.repushed < 1800 && pr.state === "queued") {
    ctx.strokeStyle = COLOURS.text;
    ctx.globalAlpha = faded * (1 - (now - pr.repushed) / 1800);
    ctx.lineWidth = 3;
    ctx.stroke();
    ctx.lineWidth = 1;
    ctx.globalAlpha = faded;
  }
  ctx.fillStyle = pr.state === "queued" ? COLOURS.text : "#08101e";
  ctx.textAlign = "center";
  ctx.fillText(String(pr.pr), cx + w / 2, cy + h / 2);
  ctx.textAlign = "left";
  let dx = cx + w - 5;
  for (const [on, colour] of [[pr.bad, COLOURS.bad], [pr.conflicts, COLOURS.warn]]) {
    if (!on) continue;
    ctx.fillStyle = colour;
    ctx.beginPath();
    ctx.arc(dx, cy + 5, 3, 0, 2 * Math.PI);
    ctx.fill();
    dx -= 7;
  }
  ctx.globalAlpha = 1;
}

function drawQueue(ctx, world, hits, now, width, top, bottom) {
  sectionLabel(ctx, `queue — ${world.queued.size} pull requests, stacks drawn from base to child`, 12, top);
  const right = width - 12;
  let x = 12;
  let y = top + 22;
  const rowH = CHIP_H + 20;
  const place = new Map();
  let hidden = 0;
  for (const tree of forest(world)) {
    const span = tree.length * (CHIP_W + GAP) + GAP;
    if (x + span > right && x > 12) {
      x = 12;
      y += rowH;
    }
    if (y + CHIP_H > bottom) {
      hidden += tree.length;
      continue;
    }
    for (const { n } of tree) {
      if (x + CHIP_W > right) {
        x = 12;
        y += rowH;
      }
      if (y + CHIP_H > bottom) {
        hidden += 1;
        continue;
      }
      place.set(n, { x, y });
      x += CHIP_W + GAP;
    }
    x += GAP * 2;
  }

  ctx.strokeStyle = COLOURS.muted;
  ctx.lineWidth = 1.2;
  for (const [n, at] of place) {
    const pr = world.prs.get(n);
    const base = pr.base !== null ? place.get(pr.base) : undefined;
    if (!base) continue;
    const x0 = base.x + CHIP_W / 2;
    const x1 = at.x + CHIP_W / 2;
    const y0 = base.y;
    const y1 = at.y;
    const lift = Math.min(16, 6 + Math.abs(x1 - x0) / 12);
    ctx.strokeStyle = pr.atomic ? COLOURS.warn : COLOURS.muted;
    ctx.beginPath();
    ctx.moveTo(x0, y0);
    ctx.bezierCurveTo(x0, Math.min(y0, y1) - lift, x1, Math.min(y0, y1) - lift, x1, y1);
    ctx.stroke();
    ctx.beginPath();
    ctx.moveTo(x1, y1);
    ctx.lineTo(x1 - 3, y1 - 5);
    ctx.lineTo(x1 + 3, y1 - 5);
    ctx.closePath();
    ctx.fillStyle = ctx.strokeStyle;
    ctx.fill();
  }
  for (const [n, at] of place) {
    const pr = world.prs.get(n);
    drawChip(ctx, pr, at.x, at.y, now, 1);
    hits.push({ x: at.x, y: at.y, w: CHIP_W, h: CHIP_H, text: () => prText(world, pr) });
  }
  if (hidden > 0) {
    ctx.fillStyle = COLOURS.muted;
    ctx.fillText(`+${hidden} more`, right - 70, bottom - 6);
  }
  return y + rowH;
}

function drawDeparted(ctx, world, hits, now, width, top) {
  sectionLabel(ctx, "left the queue — landed, ejected (white ring: an innocent), withdrawn, blocked", 12, top);
  let x = 12;
  const y = top + 14;
  for (let i = world.departed.length - 1; i >= 0; i -= 1) {
    const pr = world.departed[i];
    const age = now - pr.left;
    if (age > DEPARTED_FOR) break;
    if (x + CHIP_W > width - 12) break;
    drawChip(ctx, pr, x, y, now, 1 - 0.75 * (age / DEPARTED_FOR));
    hits.push({ x, y, w: CHIP_W, h: CHIP_H, text: () => prText(world, pr) });
    x += CHIP_W + GAP;
  }
}

function drawTrunk(ctx, world, hits, now, width, top) {
  const prs = world.count.landed;
  sectionLabel(ctx, `trunk — ${prs} landed in ${world.trunk.length} landings`, 12, top);
  const y = top + 14;
  const h = 24;
  ctx.strokeStyle = COLOURS.good;
  ctx.globalAlpha = 0.4;
  ctx.lineWidth = 2;
  ctx.beginPath();
  ctx.moveTo(12, y + h / 2);
  ctx.lineTo(width - 12, y + h / 2);
  ctx.stroke();
  ctx.lineWidth = 1;
  ctx.globalAlpha = 1;
  let x = width - 12;
  for (let i = world.trunk.length - 1; i >= 0 && x > 12; i -= 1) {
    const landing = world.trunk[i];
    const label = list(landing.members);
    const w = Math.max(ctx.measureText(label).width + 12, 18);
    x -= w;
    const age = now - landing.t;
    ctx.fillStyle = COLOURS.good;
    ctx.globalAlpha = age < 1800 ? 1 : 0.7;
    roundRect(ctx, x, y, w, h, 4);
    ctx.fill();
    if (age < 1800) {
      ctx.strokeStyle = COLOURS.text;
      ctx.globalAlpha = 1 - age / 1800;
      ctx.lineWidth = 3;
      ctx.stroke();
      ctx.lineWidth = 1;
    }
    ctx.globalAlpha = 1;
    ctx.fillStyle = "#08101e";
    ctx.fillText(fitText(ctx, label, w - 8), x + 6, y + h / 2);
    hits.push({ x, y, w, h, text: () => `batch ${landing.batch} landed at ${clockText(landing.t)}\n${list(landing.members)}` });
    x -= 3;
  }
}

function batchText(world, b) {
  const lines = [`batch ${b.id} ${list(b.members)}`, `on ${b.base === null ? "trunk" : `batch ${b.base}`}`,
    `started ${clockText(b.started)}`];
  if (b.began !== null) lines.push(`ran ${clockText(b.began)}–${b.ended === null ? "now" : clockText(b.ended)}`);
  if (b.verdict) lines.push(b.verdict + (b.broken ? " (tree broken)" : ""));
  if (b.landed) lines.push("landed");
  if (b.refused) lines.push(`refused: ${b.refused}`);
  return lines.join("\n");
}

function prText(world, pr) {
  const lines = [`#${pr.pr} arrived ${clockText(pr.arrived)}`];
  if (pr.base !== null) lines.push(`stacked on #${pr.base}`);
  if (pr.atomic) lines.push("lands only with its stack");
  if (pr.batches.size) lines.push(`in batch ${[...pr.batches].join(", ")}`);
  const truth = [pr.bad && "bad", pr.conflicts && "conflicts with trunk"].filter(Boolean);
  lines.push(`truth: ${truth.length ? truth.join(", ") : "good"}`);
  if (pr.state !== "queued") {
    let how = `${pr.state} at ${clockText(pr.left)}`;
    if (pr.state === "ejected") how += ` (${pr.justified ? "justified" : "innocent"}): ${pr.reason}`;
    if (pr.state === "blocked") how += ` on #${pr.on}`;
    lines.push(how);
  }
  return lines.join("\n");
}

// ----------------------------------------------------------------- replay

const replay = {
  doc: null,
  worlds: new Map(),
  policy: null,
  now: 0,
  end: 0,
  playing: false,
  last: null,
  hits: [],
  dirty: true,
  logIndex: -1,

  load(doc) {
    this.doc = doc;
    this.worlds = new Map();
    const traced = doc.policies.filter((p) => p.trace.length > 0);
    if (traced.length === 0) {
      $("replay").hidden = true;
      return;
    }
    $("replay").hidden = false;
    this.end = Math.max(...traced.map((p) => p.trace[p.trace.length - 1].t));
    this.now = Math.min(this.now, this.end);
    const scrubber = $("scrubber");
    scrubber.max = String(this.end);
    const switcher = $("policy-switch");
    switcher.replaceChildren();
    for (const p of traced) {
      const b = document.createElement("button");
      b.type = "button";
      b.setAttribute("role", "radio");
      b.textContent = p.name;
      b.addEventListener("click", () => this.select(p.name));
      switcher.append(b);
    }
    const keep = traced.find((p) => p.name === this.policy);
    this.select(keep ? keep.name : traced[0].name);
  },

  world() {
    let w = this.worlds.get(this.policy);
    if (!w) {
      const p = this.doc.policies.find((x) => x.name === this.policy);
      w = new World(p.trace, this.doc.config.world.runners);
      this.worlds.set(this.policy, w);
    }
    return w;
  },

  select(name) {
    this.policy = name;
    for (const b of $("policy-switch").children) b.setAttribute("aria-checked", String(b.textContent === name));
    this.logIndex = -1;
    this.dirty = true;
  },

  seek(t) {
    this.now = Math.max(0, Math.min(this.end, t));
    this.dirty = true;
  },

  toggle() {
    if (!this.playing && this.now >= this.end) this.now = 0;
    this.playing = !this.playing;
    $("play").textContent = this.playing ? "❚❚" : "▶";
    $("play").setAttribute("aria-label", this.playing ? "pause" : "play");
    this.last = null;
  },

  frame(time) {
    if (this.playing && this.doc) {
      if (this.last !== null) {
        this.now += ((time - this.last) / 1000) * Number($("speed").value);
        if (this.now >= this.end) {
          this.now = this.end;
          this.toggle();
        }
      }
      this.last = time;
      this.dirty = true;
    }
    if (this.dirty && this.doc && !$("replay").hidden) {
      this.dirty = false;
      const world = this.world();
      world.advanceTo(this.now);
      $("scrubber").value = String(Math.floor(this.now));
      $("clock").textContent = `t = ${clockText(this.now)} of ${clockText(this.end)}`;
      this.renderCounters(world);
      draw($("canvas"), world, this.hits);
      this.renderLog(world);
    }
    requestAnimationFrame((t) => this.frame(t));
  },

  renderCounters(world) {
    const c = world.count;
    const running = world.slots.filter((s) => s !== null).length;
    const items = [
      ["queued", world.queued.size], ["running", `${running}/${world.runners}`],
      ["waiting", world.waiting.length], ["runs", c.runs], ["landed", c.landed],
      ["ejected bad", c.ejected_bad], ["ejected good", c.ejected_good],
      ["blocked", c.blocked], ["withdrawn", c.withdrawn],
    ];
    $("counters").replaceChildren(...items.map(([k, v]) => {
      const span = document.createElement("span");
      span.append(`${k} `, Object.assign(document.createElement("b"), { textContent: String(v) }));
      return span;
    }));
  },

  renderLog(world) {
    if (world.index === this.logIndex) return;
    this.logIndex = world.index;
    const ol = $("log");
    const shown = [];
    for (let i = world.index - 1; i >= 0 && shown.length < 200; i -= 1) {
      const e = world.events[i];
      if (e.kind === "step" && e.land.length + e.start.length + e.eject.length + e.retire.length === 0) continue;
      shown.push(e);
    }
    shown.reverse();
    ol.replaceChildren(...shown.map((e) => {
      const li = document.createElement("li");
      li.className = `k-${e.kind}${e.kind === "run_end" && !e.passed ? " failed" : ""}`;
      li.append(Object.assign(document.createElement("span"), { className: "t", textContent: clockText(e.t) }), eventText(e));
      return li;
    }));
    ol.scrollTop = ol.scrollHeight;
  },
};

function hover(event) {
  const canvas = $("canvas");
  const rect = canvas.getBoundingClientRect();
  const x = event.clientX - rect.left;
  const y = event.clientY - rect.top;
  const tip = $("tooltip");
  const hit = replay.hits.findLast((h) => x >= h.x && x <= h.x + h.w && y >= h.y && y <= h.y + h.h);
  if (!hit) {
    tip.hidden = true;
    return;
  }
  tip.textContent = hit.text();
  tip.hidden = false;
  const left = Math.min(x + 14, rect.width - tip.offsetWidth - 4);
  tip.style.left = `${Math.max(0, left)}px`;
  tip.style.top = `${y + 14}px`;
}

$("play").addEventListener("click", () => replay.toggle());
$("scrubber").addEventListener("input", (e) => replay.seek(Number(e.target.value)));
$("canvas").addEventListener("mousemove", hover);
$("canvas").addEventListener("mouseleave", () => { $("tooltip").hidden = true; });
window.addEventListener("resize", () => { replay.dirty = true; });
document.addEventListener("keydown", (e) => {
  if (e.code === "Space" && e.target === document.body && !$("replay").hidden) {
    e.preventDefault();
    replay.toggle();
  }
});
requestAnimationFrame((t) => replay.frame(t));
boot();
