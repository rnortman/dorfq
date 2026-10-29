// The dorfq simulation page: the comparison table with headlines against
// the `github-like` column, a playground, and a replay of each policy's
// first replication. Reads the documents `dorfq-sim run --json` writes.
"use strict";

const SCHEMA = 4;
const BASELINE = "github-like";
const DEFAULT_REPORT = "busy";
const DEFAULT_REPLAY_POLICY = "bayes";

// Metric rows, as the command line's table prints them: key, decimals, and
// a one-line definition for the rows whose name does not say what they are.
// The definitions restate the field comments on `Metrics` in
// crates/dorfq-sim/src/metrics.rs and change with them.
const ROWS = [
  ["arrived", 1, null],
  ["ever_bad", 1, "pull requests that were broken against trunk at some point"],
  ["landed_bad", 1, "pull requests landed while broken against trunk, through a run that missed the break"],
  ["trunk_broken_hours", 2, "hours trunk spent broken: from a bad landing until its repair, overlapping breaks counted once, an unrepaired break cut at the replication's end"],
  ["landed", 1, null],
  ["ejected_bad", 1, "pull requests ejected, directly or with an ejected base, while broken against trunk"],
  ["ejected_good", 1, "pull requests ejected, directly or with an ejected base, while not broken against trunk"],
  ["withdrawn", 1, "pull requests withdrawn from the queue, directly or with a withdrawn base"],
  ["left_with_base", 1, "pull requests that left because their base left without landing; also counted under the base's exit"],
  ["unfinished", 1, "pull requests still open when the replication ended"],
  ["rebases", 1, null],
  ["runs", 1, "batch runs started, cancelled ones included; a batch whose build conflicted never ran"],
  ["cancelled_runs", 1, "batch runs stopped because their batch was retired"],
  ["mean_batch_size", 2, null],
  ["billed_pr_ci_runs", 1, "pull requests' own CI runs the queue caused by landing their base without them"],
  ["runner_hours", 1, "runner time of batch runs and billed PR CI runs: finished and cancelled runs' time, plus the time so far of runs still going when the replication ended; other PR CI is not in it"],
  ["billed_pr_ci_hours", 1, "the part of runner_hours spent on billed PR CI runs"],
  ["runner_hours_per_landed", 2, null],
  ["wasted_fraction", 3, "share of batch runner time spent on runs that ended without landing: failed, cancelled, refused, or passed on a dead base; runs still going when the replication ended are not waste, and PR CI is in neither side"],
  ["land_p50_hours", 2, null],
  ["land_p95_hours", 2, null],
  ["reject_p50_hours", 2, null],
  ["reject_p95_hours", 2, null],
  ["detect_p50_hours", 2, "median hours from a pull request becoming broken, or arriving broken, to its justified ejection"],
  ["detect_p95_hours", 2, "95th-percentile hours from a pull request becoming broken, or arriving broken, to its justified ejection"],
  ["notice_p50_hours", 2, "median hours from a pull request becoming broken, or arriving broken, to the queue's first notice that it is probably broken, over pull requests whose first notice said so truly"],
  ["notice_p95_hours", 2, "95th-percentile hours from a pull request becoming broken, or arriving broken, to the queue's first notice that it is probably broken, over pull requests whose first notice said so truly"],
  ["notices_good", 1, "pull requests whose first notice was wrong: told it is probably broken while not, or that it fails together with a pull request it does not"],
  ["ended_hours", 1, null],
  ["cut_off", 2, "share of replications that ended before the queue emptied"],
  ["stalled", 2, "share of replications ended by the stall detector: no landing or ejection for stall_hours"],
];

// Headline deltas against the baseline; lower is better for every one.
const HEADLINES = [
  ["runner_hours_per_landed", "runner-hours per landed pull request", 2, ""],
  ["land_p50_hours", "median time to land", 2, " h"],
  ["land_p95_hours", "95th-percentile time to land", 2, " h"],
  ["ejected_good", "good pull requests ejected", 1, ""],
  ["detect_p50_hours", "median time from breaking to ejection", 2, " h"],
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
  renderGlossary();
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
  setStatus(SERVED ? message : `${message} ${PLAYGROUND_NEEDS_HTTP}`, false);
  $("report-select").hidden = true;
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
    playground.load(doc);
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
  if (cfg.admission !== undefined) parts.push(cfg.admission);
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

function renderComparison(doc) {
  $("comparison").hidden = false;
  $("report-name").textContent = doc.name;
  $("config-text").textContent = doc.config_text;

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
  for (const [key, decimals, meaning] of ROWS) {
    const row = body.insertRow();
    const name = row.insertCell();
    name.textContent = key;
    if (meaning) name.title = meaning;
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

function renderGlossary() {
  const glossary = $("glossary");
  for (const [key, , meaning] of ROWS) {
    if (!meaning) continue;
    const term = document.createElement("dt");
    term.append(Object.assign(document.createElement("code"), { textContent: key }));
    const definition = document.createElement("dd");
    definition.textContent = meaning;
    glossary.append(term, definition);
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
      if (e.conflicts) s += " (conflicts)";
      return s;
    }
    case "broken": return `broken ${e.pr} by ${e.by}`;
    case "fixed": return `fixed ${e.pr}`;
    case "trunk_broken": return `trunk broken by ${e.batch}`;
    case "trunk_repaired": return "trunk repaired";
    case "repush": return `repush ${e.pr}`;
    case "withdraw": return `withdraw ${e.pr}`;
    case "merged": return `merged ${e.pr}`;
    case "lost": return `lost ${e.pr}`;
    case "blocked": return `blocked ${e.pr} ${e.final ? "on" : "with"} ${e.on}${e.bad ? " bad" : ""}`;
    case "start": return `start ${e.batch} ${e.members.length === 0 ? "probe" : list(e.members)} on ${e.base === null ? "trunk" : e.base}`;
    case "run_end":
      return `ci ${e.batch} ${e.passed ? "passed" : "failed"}${e.broken ? " broken" : ""}${e.flake ? " flake" : ""}${e.outage ? " outage" : ""}`;
    case "land": return `land ${e.batch} ${list(e.members)}`;
    case "refused": return `refused ${e.batch}: ${e.reason}`;
    // A journal event carries no `justified`; only a simulated one does.
    case "eject":
      return `eject ${e.pr}${e.justified === true ? " justified" : e.justified === false ? " unjustified" : ""}: ${e.reason ?? "reason unknown"}${e.final ? "" : ", rebasing"}`;
    case "notice":
      return `notice ${e.pr}${e.justified === true ? " justified" : e.justified === false ? " unjustified" : ""}: ${e.reason}`;
    case "retire": return `retire ${e.batch}${e.cancelled ? " cancelled" : ""}`;
    case "pr_ci": return `pr-ci ${e.pr}${e.billed ? " billed" : ""}`;
    case "pr_ci_end": return `pr-ci ${e.pr} ${e.cancelled ? "cancelled" : e.failed ? "failed" : "passed"}`;
    case "rebase": return `rebase ${e.pr} ${e.why === "base_landed" ? "base landed" : e.why}`;
    case "rerun": return `re-run ${e.pr}`;
    case "admit": return `admit ${e.pr}${e.base === null ? "" : ` on ${e.base}`}`;
    case "recheck": return `recheck ${e.pr}`;
    case "wait": return `wait ${e.pr} on ${e.on}`;
    case "step": {
      let s = `queued=${e.queued} in_flight=${e.in_flight}`;
      if (e.land.length) s += ` | land ${list(e.land)}`;
      if (e.notify.length) s += ` | notify ${e.notify.map((x) => `${x.pr} (${x.reason})`).join(" ")}`;
      if (e.start.length) s += ` | start ${e.start.map((x) => `${x.batch}${list(x.members)}`).join(" ")}`;
      if (e.eject.length) s += ` | eject ${e.eject.map((x) => `${x.pr} (${x.reason})`).join(" ")}`;
      if (e.retire.length) s += ` | retire ${list(e.retire)}`;
      return s;
    }
    // TODO(correctness-trace-pane-drops-real-only-event-detail): print the
    // fields of a journal's error, restart, conflict, conflicting, held and
    // trunk events.
    default: return e.kind;
  }
}

// Why an arrived pull request is outside the queue, by the event that put
// it there: in full, and short enough to sit under a chip.
const OUTSIDE = {
  pr_ci: { long: "PR CI", short: "PR CI" },
  rebasing: { long: "rebasing", short: "rebasing" },
  fixing: { long: "its author is fixing it", short: "fixing" },
  rerun: { long: "waiting to re-run its CI", short: "re-run" },
  with_base: { long: "out with base", short: "with base" },
  waiting_for_base: { long: "waiting for base", short: "for base" },
};

// Ends `batch` with the verdict of `e`, a run_end, retire or conflict event.
function endBatch(batch, e) {
  batch.ended = e.t;
  if (e.kind === "conflict") {
    batch.verdict = "conflict";
  } else if (e.kind === "retire") {
    batch.verdict = "cancelled";
  } else {
    batch.verdict = e.passed ? "passed" : e.flake ? "flake" : "failed";
    batch.broken = e.broken;
    batch.checks = e.checks;
  }
}

// One policy's trace folded into the world as it stood at a time. Each of
// the policy's lanes holds one running batch; a pull request's own CI run
// takes no lane.
class World {
  constructor(events, lanes) {
    this.events = events;
    this.lanes = lanes;
    this.reset();
  }

  reset() {
    this.index = 0;
    this.now = 0;
    this.prs = new Map();
    this.queued = new Set();
    this.batches = new Map();
    this.slots = new Array(this.lanes).fill(null);
    this.runs = [];
    this.trunk = [];
    this.departed = [];
    this.unresolved = [];
    this.count = {
      landed: 0, ejected: 0, ejected_bad: 0, ejected_good: 0, withdrawn: 0, left_with_base: 0,
      merged: 0, runs: 0, billed_pr_ci_runs: 0,
    };
  }

  // Bad against trunk: broken itself, or, while still here, stacked on a
  // base that is here and bad. A departed one keeps the truth it left with.
  bad(pr) {
    if (pr.bad || pr.left !== null || pr.base === null) return pr.bad;
    const base = this.prs.get(pr.base);
    return Boolean(base) && base.left === null && this.bad(base);
  }

  // The departure a pull request that left with its base counts under: the
  // first down its chain of bases that is not a block.
  rootDeparture(on) {
    let base = this.prs.get(on);
    while (base && base.state === "blocked") base = this.prs.get(base.on);
    return base ? base.state : null;
  }

  // Counts an ejection under its truth: bad when `bad` is true, good when it
  // is false, neither when it is null (a journal carries no truth).
  countEjected(bad) {
    this.count.ejected += 1;
    if (bad === true) this.count.ejected_bad += 1;
    else if (bad === false) this.count.ejected_good += 1;
  }

  // Counts each final block under its root departure, with its own truth,
  // once that root has departed: a base the same plan ejects after its
  // child's block departs later in the trace.
  resolveBlocked() {
    this.unresolved = this.unresolved.filter(({ on, bad }) => {
      const root = this.rootDeparture(on);
      if (root === "ejected") {
        this.countEjected(bad);
      } else if (root === "withdrawn") {
        this.count.withdrawn += 1;
      } else {
        return true;
      }
      return false;
    });
  }

  // A queued pull request whose current head's PR CI has not passed: the
  // forge reports it waiting, and no batch can take it.
  held(pr) {
    return this.queued.has(pr.pr) && (pr.ci !== null || pr.fixing || pr.rerunning);
  }

  heldCount() {
    return [...this.queued].filter((n) => this.held(this.prs.get(n))).length;
  }

  // Every arrived pull request neither in the queue nor departed, by number.
  outside() {
    return [...this.prs.values()]
      .filter((pr) => pr.left === null && !this.queued.has(pr.pr))
      .sort((a, b) => a.pr - b.pr);
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

  freeSlot(job) {
    const i = this.slots.indexOf(job);
    if (i >= 0) this.slots[i] = null;
  }

  // A plan lands, starts, ejects and retires in that order, so a start can
  // precede, at the same instant, the retire or run end that frees its lane.
  // With no lane free, the batch the first such later event ends is ended
  // now, as that event will end it, and its lane taken; only if there is
  // none does the picture grow a lane.
  takeSlot(batch) {
    let slot = this.slots.indexOf(null);
    if (slot < 0) {
      const ending = this.endingNow(batch.started);
      if (ending !== null) {
        slot = ending.lane;
        this.slots[slot] = null;
      }
    }
    if (slot < 0) {
      slot = this.slots.length;
      this.slots.push(null);
    }
    this.slots[slot] = batch;
    batch.lane = slot;
    this.runs.push(batch);
  }

  // The running batch that a retire or run end later at instant `t` ends,
  // ended with that event's verdict; null if there is none.
  endingNow(t) {
    for (let i = this.index + 1; i < this.events.length && this.events[i].t === t; i += 1) {
      const e = this.events[i];
      if (e.kind !== "retire" && e.kind !== "run_end") continue;
      const batch = this.batches.get(e.batch);
      if (!batch || batch.ended !== null || batch.lane === null) continue;
      endBatch(batch, e);
      return batch;
    }
    return null;
  }

  apply(e) {
    switch (e.kind) {
      case "arrive":
        this.prs.set(e.pr, {
          pr: e.pr, base: e.base, atomic: e.atomic, arrived: e.t, state: "queued", left: null,
          bad: false, conflicts: e.conflicts ?? null,
          batches: new Set(), repushed: null, ci: null, place: null, rebases: 0, fixing: false,
          rerunning: false,
        });
        break;
      case "admit": {
        const pr = this.prs.get(e.pr);
        if (!pr) break;
        pr.base = e.base;
        pr.place = null;
        this.queued.add(e.pr);
        break;
      }
      case "recheck":
        this.queued.delete(e.pr);
        break;
      case "broken": {
        const pr = this.prs.get(e.pr);
        if (pr) pr.bad = true;
        break;
      }
      case "fixed": {
        const pr = this.prs.get(e.pr);
        if (pr) pr.bad = false;
        break;
      }
      case "rebase": {
        const pr = this.prs.get(e.pr);
        if (!pr) break;
        // Its own CI failed: its author fixes it where it is, held in the
        // queue under `label`, out checking under `github`.
        pr.fixing = e.why === "broken";
        pr.rerunning = false;
        pr.place = pr.fixing ? "fixing" : "rebasing";
        pr.rebases += 1;
        break;
      }
      case "rerun": {
        const pr = this.prs.get(e.pr);
        if (!pr) break;
        // Its own CI failed for trunk or an outage: its author re-runs it on
        // the same head once CI is back and trunk is green, held meanwhile.
        pr.rerunning = true;
        pr.place = "rerun";
        break;
      }
      case "wait": {
        const pr = this.prs.get(e.pr);
        if (!pr) break;
        pr.place = "waiting_for_base";
        pr.on = e.on;
        break;
      }
      case "pr_ci": {
        const pr = this.prs.get(e.pr);
        if (!pr) break;
        pr.ci = { billed: e.billed };
        pr.place = "pr_ci";
        // A run on a head not broken against trunk is the pending re-run.
        if (!pr.bad) pr.rerunning = false;
        if (e.billed) this.count.billed_pr_ci_runs += 1;
        break;
      }
      case "pr_ci_end": {
        const pr = this.prs.get(e.pr);
        if (pr) pr.ci = null;
        break;
      }
      case "repush": {
        const pr = this.prs.get(e.pr);
        if (!pr) break;
        pr.repushed = e.t;
        pr.fixing = false;
        pr.rerunning = false;
        break;
      }
      case "withdraw":
        this.count.withdrawn += 1;
        this.depart(e.pr, "withdrawn", e.t, {});
        this.resolveBlocked();
        break;
      case "merged":
        this.count.merged += 1;
        this.depart(e.pr, "merged", e.t, {});
        break;
      case "lost":
        this.depart(e.pr, "lost", e.t, {});
        break;
      case "blocked":
        if (e.final) {
          this.count.left_with_base += 1;
          const bad = e.bad ?? null;
          this.depart(e.pr, "blocked", e.t, { on: e.on, bad });
          this.unresolved.push({ on: e.on, bad });
          this.resolveBlocked();
        } else {
          const pr = this.prs.get(e.pr);
          if (!pr) break;
          pr.place = "with_base";
          pr.on = e.on;
          pr.rerunning = false;
          this.queued.delete(e.pr);
        }
        break;
      case "start": {
        const batch = {
          id: e.batch, base: e.base, members: e.members, started: e.t,
          ended: null, lane: null, verdict: null, landed: false,
        };
        this.batches.set(e.batch, batch);
        this.takeSlot(batch);
        this.count.runs += 1;
        for (const m of e.members) {
          const pr = this.prs.get(m);
          if (pr) pr.batches.add(e.batch);
        }
        break;
      }
      case "run_end": {
        const batch = this.batches.get(e.batch);
        if (!batch || batch.ended !== null) break;
        endBatch(batch, e);
        this.freeSlot(batch);
        break;
      }
      // A batch whose build conflicted never ran CI, so it is not a run.
      case "conflict": {
        const batch = this.batches.get(e.batch);
        if (!batch || batch.ended !== null) break;
        endBatch(batch, e);
        this.freeSlot(batch);
        this.count.runs -= 1;
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
        if (!e.final) {
          this.queued.delete(e.pr);
          break;
        }
        const justified = e.justified ?? null;
        this.countEjected(justified);
        this.depart(e.pr, "ejected", e.t, { reason: e.reason, justified });
        this.resolveBlocked();
        break;
      }
      case "retire": {
        const batch = this.batches.get(e.batch);
        if (!batch) break;
        batch.retired = e.t;
        if (batch.ended === null) {
          endBatch(batch, e);
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

const VERDICT = {
  passed: COLOURS.good, failed: COLOURS.bad, flake: COLOURS.flake, cancelled: COLOURS.cancel,
  conflict: COLOURS.warn,
};
const DEPARTURE = {
  landed: COLOURS.good, ejected: COLOURS.bad, withdrawn: COLOURS.cancel, blocked: COLOURS.warn,
  merged: COLOURS.good, lost: COLOURS.cancel,
};

const WINDOW = 6 * 3600;
const LEFT = 92;
const ROW = 26;
const CHIP_W = 46;
const CHIP_H = 22;
const GAP = 6;
const DEPARTED_FOR = 3 * 3600;
// The canvas, top to bottom: the lanes' header, the lanes, the gap before the
// queue, the queue's band, and the outside, departed and trunk strips,
// anchored to the bottom.
const LANES_TOP = 26;
const QUEUE_GAP = 14;
const QUEUE_BAND = 330;
const BOTTOM_STRIPS = 212;

// The canvas height that gives `lanes` lanes and a queue band of fixed height.
function canvasHeight(lanes) {
  return LANES_TOP + lanes * ROW + QUEUE_GAP + QUEUE_BAND + BOTTOM_STRIPS;
}

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
  const y = drawLanes(ctx, world, hits, now, width);
  drawQueue(ctx, world, hits, now, width, y + QUEUE_GAP, height - BOTTOM_STRIPS);
  drawOutside(ctx, world, hits, now, width, height - 196);
  drawDeparted(ctx, world, hits, now, width, height - 128);
  drawTrunk(ctx, world, hits, now, width, height - 56);
}

function sectionLabel(ctx, text, x, y) {
  ctx.fillStyle = COLOURS.muted;
  ctx.font = "11px system-ui, sans-serif";
  ctx.fillText(text.toUpperCase(), x, y);
  ctx.font = "12px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
}

function drawLanes(ctx, world, hits, now, width) {
  const top = LANES_TOP;
  const right = width - 12;
  const x = (t) => LEFT + ((t - (now - WINDOW)) / WINDOW) * (right - LEFT);
  sectionLabel(ctx, "batches", 12, 12);

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
    ctx.fillText(`lane ${i + 1}`, 12, ry + ROW / 2);
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
      if (b.started < now - 2 * WINDOW) break;
      continue;
    }
    const bx = Math.max(x(b.started), LEFT - 4);
    const bw = Math.max(x(end) - bx, 2);
    const by = top + b.lane * ROW + 3;
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
  return top + world.slots.length * ROW;
}

// `look` is "queue" for a chip in the queue, "outside" for one on the
// outside strip, and "departed" for one that left.
function drawChip(ctx, world, pr, x, y, now, faded, look) {
  const inBatch = pr.batches.size > 0;
  const held = look === "queue" && world.held(pr);
  const age = now - pr.arrived;
  const pop = pr.state === "queued" && age < 1200 ? 1 + 0.25 * (1 - age / 1200) : 1;
  const w = CHIP_W * pop;
  const h = CHIP_H * pop;
  const cx = x - (w - CHIP_W) / 2;
  const cy = y - (h - CHIP_H) / 2;
  ctx.globalAlpha = faded * (held || look === "outside" ? 0.55 : 1);
  ctx.fillStyle = pr.state === "queued" ? (inBatch ? "#22385a" : COLOURS.chip) : DEPARTURE[pr.state];
  roundRect(ctx, cx, cy, w, h, 5);
  ctx.fill();
  ctx.globalAlpha = faded;
  if (held || look === "outside") {
    ctx.strokeStyle = COLOURS.muted;
    ctx.setLineDash(held ? [2, 2] : [5, 3]);
    ctx.lineWidth = 1.2;
    ctx.stroke();
    ctx.setLineDash([]);
    ctx.lineWidth = 1;
  }
  if (pr.state === "queued" && (inBatch || pr.atomic)) {
    ctx.strokeStyle = pr.atomic ? COLOURS.warn : COLOURS.accent;
    ctx.lineWidth = 2;
    if (pr.atomic && inBatch) ctx.setLineDash([4, 2]);
    ctx.stroke();
    ctx.setLineDash([]);
    ctx.lineWidth = 1;
  }
  if (pr.state === "ejected" && pr.justified === false) {
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
  for (const [on, colour] of [[world.bad(pr), COLOURS.bad], [pr.conflicts, COLOURS.warn]]) {
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
  const held = world.heldCount();
  sectionLabel(ctx, `queue — ${world.queued.size} pull requests, ${held} held for their own CI, stacks drawn from base to child`, 12, top);
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
    drawChip(ctx, world, pr, at.x, at.y, now, 1, "queue");
    hits.push({ x: at.x, y: at.y, w: CHIP_W, h: CHIP_H, text: () => prText(world, pr) });
  }
  if (hidden > 0) {
    ctx.fillStyle = COLOURS.muted;
    ctx.fillText(`+${hidden} more`, right - 70, bottom - 6);
  }
  return y + rowH;
}

function drawOutside(ctx, world, hits, now, width, top) {
  const outside = world.outside();
  sectionLabel(ctx, `outside the queue — ${outside.length}: ${Object.values(OUTSIDE).map((o) => o.long).join(", ")}`, 12, top);
  const right = width - 12;
  const y = top + 14;
  let x = 12;
  for (const pr of outside) {
    if (x + CHIP_W > right) {
      ctx.fillStyle = COLOURS.muted;
      ctx.fillText(`+${outside.length - outside.indexOf(pr)}`, x, y + CHIP_H / 2);
      break;
    }
    drawChip(ctx, world, pr, x, y, now, 1, "outside");
    ctx.fillStyle = COLOURS.muted;
    ctx.font = "9px system-ui, sans-serif";
    ctx.textAlign = "center";
    ctx.fillText(fitText(ctx, OUTSIDE[pr.place]?.short || "", CHIP_W + GAP), x + CHIP_W / 2, y + CHIP_H + 8);
    ctx.textAlign = "left";
    ctx.font = "12px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
    hits.push({ x, y, w: CHIP_W, h: CHIP_H, text: () => prText(world, pr) });
    x += CHIP_W + GAP;
  }
}

function drawDeparted(ctx, world, hits, now, width, top) {
  sectionLabel(ctx, "left the queue — landed, ejected (white ring: an innocent), withdrawn, left with its base", 12, top);
  let x = 12;
  const y = top + 14;
  for (let i = world.departed.length - 1; i >= 0; i -= 1) {
    const pr = world.departed[i];
    const age = now - pr.left;
    if (age > DEPARTED_FOR) break;
    if (x + CHIP_W > width - 12) break;
    drawChip(ctx, world, pr, x, y, now, 1 - 0.75 * (age / DEPARTED_FOR), "departed");
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
    `ran ${clockText(b.started)}–${b.ended === null ? "now" : clockText(b.ended)}`];
  if (b.verdict) lines.push(b.verdict + (b.broken ? " (tree broken)" : ""));
  if (b.landed) lines.push("landed");
  if (b.refused) lines.push(`refused: ${b.refused}`);
  if (b.checks && b.checks.length) lines.push(`failed: ${b.checks.join(", ")}`);
  return lines.join("\n");
}

function prText(world, pr) {
  const lines = [`#${pr.pr} arrived ${clockText(pr.arrived)}`];
  if (pr.base !== null) lines.push(`stacked on #${pr.base}`);
  if (pr.atomic) lines.push("lands only with its stack");
  if (pr.batches.size) lines.push(`in batch ${[...pr.batches].join(", ")}`);
  if (pr.state === "queued") {
    if (world.queued.has(pr.pr)) {
      if (world.held(pr)) {
        if (pr.fixing) lines.push("held: its own CI failed; its author is fixing it");
        else if (pr.rerunning) lines.push("held: its own CI failed for trunk or an outage; its author will re-run it once CI is back and trunk is green");
        else lines.push(`held: its own CI has not passed on this head${pr.ci.billed ? " (a re-run billed to the queue)" : ""}`);
      }
    } else {
      let where = `outside the queue: ${OUTSIDE[pr.place]?.long || "-"}`;
      if (pr.place === "with_base" || pr.place === "waiting_for_base") where += ` #${pr.on}`;
      lines.push(where);
    }
  }
  if (pr.rebases) lines.push(`rebased ${pr.rebases} time${pr.rebases === 1 ? "" : "s"}`);
  // A journal's arrival carries no `conflicts`, and its document no truth.
  if (pr.conflicts === null) {
    lines.push("truth: unknown");
  } else {
    const truth = [world.bad(pr) && "broken against trunk", pr.conflicts && "conflicts with trunk"].filter(Boolean);
    lines.push(`truth: ${truth.length ? truth.join(", ") : "good"}`);
  }
  if (pr.state !== "queued") {
    let how = `${pr.state} at ${clockText(pr.left)}`;
    if (pr.state === "ejected") {
      const truth = pr.justified === null ? "truth unknown" : pr.justified ? "justified" : "innocent";
      how += ` (${truth}): ${pr.reason ?? "reason unknown"}`;
    }
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
    const preferred = traced.find((p) => p.name === DEFAULT_REPLAY_POLICY);
    this.select((keep ?? preferred ?? traced[0]).name);
  },

  world() {
    let w = this.worlds.get(this.policy);
    if (!w) {
      const p = this.doc.policies.find((x) => x.name === this.policy);
      w = new World(p.trace, policyConfig(this.doc, this.policy).lanes ?? 1);
      this.worlds.set(this.policy, w);
    }
    return w;
  },

  select(name) {
    this.policy = name;
    $("stage").style.setProperty("--stage-height", `${canvasHeight(this.world().lanes)}px`);
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
      ["queued", world.queued.size], ["held", world.heldCount()],
      ["outside", world.outside().length], ["running", `${running}/${world.slots.length}`],
      ["runs", c.runs], ["PR CI billed", c.billed_pr_ci_runs], ["landed", c.landed],
      ["ejected", c.ejected], ["ejected bad", c.ejected_bad], ["ejected good", c.ejected_good],
      ["withdrawn", c.withdrawn], ["left with base", c.left_with_base], ["merged outside", c.merged],
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
      if (e.kind === "step" && e.land.length + e.notify.length + e.start.length + e.eject.length + e.retire.length === 0) continue;
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

// ---- the playground ------------------------------------------------------

const PLAYGROUND_NEEDS_HTTP = "The playground needs the page served over HTTP.";
// Opened from a file, the browser starts no worker and fetches no module.
const SERVED = location.protocol !== "file:";

// Runs an edited configuration through the simulator compiled to
// WebAssembly, in a worker, and shows the result as any other report. A run
// cannot be interrupted, so cancel terminates the worker; the next run
// starts a fresh one, as it does after a crash, which leaves the module's
// instance unusable.
const playground = {
  worker: null,
  started: null,
  ticker: null,
  running: false,
  // The text of the last document a run produced, kept for download.
  documentText: null,

  load(doc) {
    $("playground").hidden = false;
    $("playground-text").value = doc.config_text;
    $("playground-offline").hidden = SERVED;
    $("playground-run").disabled = !SERVED || this.running;
  },

  status(text, error) {
    const el = $("playground-status");
    el.textContent = text;
    el.classList.toggle("error", Boolean(error));
  },

  run() {
    if (this.running) return;
    this.running = true;
    if (this.worker === null) {
      this.worker = new Worker("sim-worker.js");
      this.worker.onmessage = (event) => this.reply(event.data);
      this.worker.onerror = (event) => {
        this.finish();
        this.drop();
        this.status(`The simulator's worker failed: ${event.message}`, true);
      };
    }
    this.started = performance.now();
    $("playground-run").disabled = true;
    $("playground-cancel").disabled = false;
    this.tick();
    this.ticker = setInterval(() => this.tick(), 200);
    this.worker.postMessage($("playground-text").value);
  },

  tick() {
    this.status(`running… ${this.elapsed()}`);
  },

  elapsed() {
    return `${((performance.now() - this.started) / 1000).toFixed(1)} s`;
  },

  finish() {
    this.running = false;
    clearInterval(this.ticker);
    $("playground-run").disabled = !SERVED;
    $("playground-cancel").disabled = true;
  },

  drop() {
    if (this.worker !== null) this.worker.terminate();
    this.worker = null;
  },

  cancel() {
    this.finish();
    this.drop();
    this.status(`cancelled after ${this.elapsed()}`);
  },

  // The object URL is revoked only after the click has started the download.
  download() {
    if (this.documentText === null) return;
    const blob = new Blob([this.documentText], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = "playground.json";
    document.body.append(link);
    link.click();
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 0);
  },

  reply(message) {
    this.finish();
    switch (message.kind) {
      case "document": {
        const took = this.elapsed();
        let doc;
        try {
          doc = JSON.parse(message.text);
        } catch (error) {
          this.status(`The simulator's document is not JSON: ${error.message}`, true);
          break;
        }
        this.documentText = message.text;
        $("playground-download").disabled = false;
        show(doc);
        this.status(
          `ran in ${took}. The browser's floating point may differ from the ` +
            "published reports' in the last place, so its numbers can too.",
        );
        break;
      }
      case "error":
        this.status(message.message, true);
        break;
      case "crash":
        this.drop();
        this.status(`the simulator crashed: ${message.message}`, true);
        break;
      case "unavailable":
        this.drop();
        this.status(`Could not load the simulator (${message.message}). ${PLAYGROUND_NEEDS_HTTP}`, true);
        break;
      default:
        this.status(`The simulator's worker replied ${JSON.stringify(message.kind)}.`, true);
    }
  },
};

$("playground-run").addEventListener("click", () => playground.run());
$("playground-cancel").addEventListener("click", () => playground.cancel());
$("playground-download").addEventListener("click", () => playground.download());
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
