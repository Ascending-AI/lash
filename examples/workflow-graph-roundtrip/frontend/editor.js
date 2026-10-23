import {
  buildDataflow,
  availableName,
  planInputEdit,
  encodeValue as expression,
  sourceOptions,
  typeName,
  expressionName,
  constantValue,
  constantText,
  fieldsOf,
} from "./dataflow.mjs";
import {
  actionOf,
  effectOf,
  recordInput as canonicalRecordInput,
  literal,
  startPosition as scopeStart,
  appendPosition,
  summary,
  viewOf,
} from "./projection.mjs";
import { approvalControl } from "./approvals.mjs";
import {
  armCaption,
  captionFrom,
  captionSite,
  conditionText,
  nodeCaptions,
  withCaption,
  workflowTitle,
} from "./captions.mjs";
import {
  highlightLine,
  kernelLinking,
  readableLinking,
  sourceLinking,
} from "./codepane.mjs";
import {
  clampZoom,
  dragged,
  frame,
  neighbour,
  pinchStep,
  wheelFactor,
  wheelIntent,
  zoomAt,
} from "./viewport.mjs";
import { NOTHING, close, escape, open, select } from "./selection.mjs";
import { armsTaken, chip, counts, nodeState, replayAt } from "./runview.mjs";
const $ = (id) => document.getElementById(id);
const approvals = approvalControl(
  $("approve"),
  (key) => call(`/approvals/${encodeURIComponent(key)}`, { approved: true }),
  (error) => report(errorText(error), true),
);
const key = (site) => JSON.stringify([site.unit, site.path ?? []]);
let workflow,
  examples = [],
  // The dialect used to open examples that declare a source in that language.
  dialect = "typescript",
  codeView = "kernel",
  codeLinking,
  calls = new Map(),
  runInputs = {},
  dataView,
  connecting,
  finalOutput,
  observationGap = false,
  // The selected item and whether its inspector is open (selection.mjs),
  // and the statement it shows.
  focus = NOTHING,
  selected,
  busy = false,
  running = false;
let statuses = new Map(),
  statementNodes = new Map();
// The run on the canvas: lash's site statuses (and their errors), coverage
// and path, the step the replay scrubber shows, and the trace rows.
let errors = new Map(),
  runCounts = counts(undefined),
  runPath,
  replay,
  runEnded = false,
  runStarted = 0,
  following = false,
  traceRows = [];
const presets = {
  "display.show_message": { text: "Hello from the workbench" },
  "display.set_status": { key: "workflow", value: "Ready" },
  "gmail.list_recent": { count: 3 },
  "web.search": { query: "workflow authoring" },
  "research.search": { topic: "AI support agents", max_sources: 3 },
  "metrics.fetch": { workspace: "Acme", period: "current" },
};
async function call(path, body) {
  const response = await fetch(
    path,
    body === undefined
      ? {}
      : {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(body),
        },
  );
  const answer = await response.json();
  if (!response.ok) throw answer.error ?? { message: response.statusText };
  return answer;
}
function report(message, error = false) {
  $("result").textContent = message;
  $("result").className = `result ${error ? "error" : "ok"}`;
  document.querySelectorAll(".form-feedback").forEach((node) => node.remove());
  const surface =
    document.querySelector("dialog[open]") ??
    (!$("node-panel").hidden ? $("node-form") : null);
  if (error && surface) {
    const feedback = document.createElement("p");
    feedback.className = "form-feedback";
    feedback.setAttribute("role", "alert");
    feedback.textContent = message;
    surface.append(feedback);
  }
}
function errorText(error) {
  return (
    error?.details?.diagnostics?.map((d) => d.message).join("\n") ||
    error?.message ||
    String(error)
  );
}
function controls() {
  document
    .querySelectorAll(
      "[data-write], #node-panel .save-node, .add-port",
    )
    .forEach((button) => (button.disabled = busy || running || !workflow));
  $("run").disabled = busy || running || !workflow?.definition;
}
async function action(work) {
  busy = true;
  controls();
  try {
    await work();
  } catch (error) {
    report(errorText(error), true);
  } finally {
    busy = false;
    controls();
  }
}
function show(next, correspondence) {
  $("node-panel").hidden = true;
  $("node-form").replaceChildren();
  const old = selected?.site;
  const previousScope = activeScope;
  const sameEntry = workflow?.entry === next.entry;
  workflow = next;
  if (!sameEntry || !correspondence) runInputs = initialInputs(next);
  connecting = undefined;
  resetRun();
  statementNodes = new Map(
    viewOf(next).statements.map((item) => [key(item.site), item.statement]),
  );
  const mapped =
    old &&
    correspondence?.entries?.find((item) => key(item.from) === key(old))?.to;
  const kept = correspondence ? mapped : old;
  const statement =
    kept &&
    viewOf(next).statements.find((stmt) => key(stmt.site) === key(kept));
  focus = statement ? select(NOTHING, key(statement.site)) : NOTHING;
  selected = statement;
  $("revision").textContent = "All changes saved";
  $("admission").textContent = next.definition
    ? "Ready to run"
    : `Not admitted: ${next.notAdmitted}`;
  $("source-kind").value = next.authored?.version === next.version ? "authored" : "printed";
  $("source-dialect").value = next.sourceDialect;
  refreshSourceEditor();
  renderCode();
  $("source").scrollTop = 0;
  $("source").scrollLeft = 0;
  $("entry").value = next.entry;
  const mappedScope =
    previousScope &&
    correspondence?.entries?.find(
      (item) => key(item.from) === key(previousScope),
    )?.to;
  const scopes = canvasScopes();
  activeScope = scopes.find(
    (scope) => mappedScope && key(scope.block) === key(mappedScope),
  )?.block ?? scopes[0]?.block;
  renderScopes();
  renderCanvas();
  renderSelection();
  controls();
}
function renderSelection(reveal) { markCode(reveal); }
$("import").onclick = () =>
  action(async () => {
    const next = await call("/workflow/source", {
      version: workflow.version,
      source: $("source").value,
      entry: $("entry").value,
      dialect: $("source-dialect").value,
    });
    selected = undefined;
    show(next);
    $("example").value = "";
    report(`Imported ${dialectName(workflow.sourceDialect)} · revision ${workflow.version}.`);
  });
async function loadExample(id) {
  const example = examples.find((entry) => entry.id === id);
  if (!example || !(dialect === "python" ? example.pythonSource : example.typescriptSource)) return;
  await action(async () => {
    if (example.group !== "module") {
      $("corpus-title").textContent = example.name;
      $("corpus-note").textContent = example.refusal
        ? `Module import refuses ${example.refusal}. The saved workflow is unchanged.`
        : "Cell code runs in a session. Its recorded crossing is shown below.";
      $("corpus-source").textContent = dialect === "python" ? example.pythonSource : example.typescriptSource;
      $("corpus-witness").textContent = (dialect === "python" ? example.pythonWitness : example.typescriptWitness) ?? example.refusal;
      $("corpus-case").showModal();
      return;
    }
    const next = await call("/workflow/select", { id, dialect });
    selected = undefined;
    show(next);
    fit();
    report(`Loaded ${example.name} · ${dialectName(dialect)}.`);
  });
  $("example").value = id;
  renderDialect();
}
const dialectName = (name) => (name === "python" ? "Python" : "TypeScript");
function setDialect(next) {
  dialect = next;
  renderExamples();
  renderDialect();
}
// The toggle chooses the dialect for the next example opened.
function renderDialect() {
  for (const name of ["typescript", "python"]) {
    $(`dialect-${name}`).setAttribute("aria-pressed", String(dialect === name));
  }
  $("source").setAttribute("aria-label", `${dialectName($("source-dialect").value)} source`);
}
function chooseDialect(next) {
  if (next === dialect) return;
  setDialect(next);
  const id = $("example").value;
  if (id) loadExample(id);
}
// One option per example, grouped, with the size of its lowering in the
// chosen dialect.
function renderExamples() {
  const selected = $("example").value;
  const groups = { module: "Workflow modules", cell: "Cell crossings", refusal: "Module refusals" };
  const placeholder = el("option", null, "Choose an example…");
  placeholder.value = "";
  placeholder.disabled = true;
  $("example").replaceChildren(
    placeholder,
    ...Object.entries(groups).map(([group, label]) => {
      const optgroup = el("optgroup");
      optgroup.label = label;
      optgroup.append(
        ...examples
          .filter((example) => example.group === group)
          .map((example) => {
            const available = Boolean(dialect === "python" ? example.pythonSource : example.typescriptSource);
            const size = dialect === "python" && example.python ? example.python : example;
            const option = el(
              "option",
              null,
              example.group === "module" ? `${example.name} · ${size.statements} statements · ${size.scopes} scopes` : example.name,
            );
            option.value = example.id;
            option.disabled = !available;
            if (!available) option.textContent = `${example.name} · no ${dialectName(dialect)} source`;
            option.title = available ? example.description : `This example has no ${dialectName(dialect)} source.`;
            return option;
          }),
      );
      return optgroup;
    }),
  );
  const selectedExample = examples.find((example) => example.id === selected);
  const selectedSource = dialect === "python" ? selectedExample?.pythonSource : selectedExample?.typescriptSource;
  $("example").value = selectedSource ? selected : "";
}
// The run on the canvas and in the trace panel. What it shows comes from
// the run's events: lash's overlay statuses, coverage and replayed path.
function resetRun() {
  approvals.reset();
  statuses = new Map();
  errors = new Map();
  calls = new Map();
  runCounts = counts(undefined);
  runPath = undefined;
  replay = undefined;
  runEnded = false;
  following = false;
  finalOutput = undefined;
  observationGap = false;
  traceRows = [];
  $("run-status").textContent = "Not started";
  renderTrace();
  renderScrubber();
}
function runView() {
  return { statuses, errors, counts: runCounts, ended: runEnded, replay };
}
const elapsed = () => Math.round(performance.now() - runStarted);
// The statement an event's site is: the action site a run reports, or the
// statement's own site.
function statementAt(site) {
  const node = viewOf(workflow).nodes.find((node) => key(node.site) === key(site));
  if (!node) return;
  const owner = viewOf(workflow).statements.find((statement) => key(statement.site) === key(node.statement));
  return { ...owner, site: node.site, block: node.block, statement_site: owner.site, node_kind: node.kind,
    actions: node.kind === "action" ? [node.site] : [] };
}

function traceNote(note, error = false) {
  traceRows.push({ at: elapsed(), note, error });
}
// A site's status opens a trace row, and its outcome ends the row.
function traceStatus(event) {
  const site = key(event.site);
  let row = traceRows.findLast(
    (row) => row.site === site && row.duration === undefined,
  );
  if (!row) {
    const statement = statementAt(event.site);
    row = {
      at: elapsed(),
      site,
      item: statement && key(statement.site),
      title: statement ? summary(statement.statement) : "Step",
      tool: (statement && effectOf(workflow, statement)) ?? "",
    };
    traceRows.push(row);
  }
  row.status = event.status;
  if (event.status === "failed") row.error = event.error ?? "failed";
  if (!["started", "waiting"].includes(event.status))
    row.duration = elapsed() - row.at;
}
function traceCall(event) {
  const site = key(event.site);
  const row = traceRows.findLast(
    (row) => row.site === site && (row.callId ?? event.callId) === event.callId,
  );
  if (!row) return;
  row.callId = event.callId;
  if (event.call) row.result = chip(event.call.output, 60);
}
function renderTrace() {
  const body = $("trace-rows");
  if (!traceRows.length) {
    const empty = el("tr", "empty");
    const cell = el("td", null, "Run the workflow to see its steps here.");
    cell.colSpan = 5;
    empty.append(cell);
    body.replaceChildren(empty);
    return;
  }
  body.replaceChildren(
    ...traceRows.map((row) => {
      const line = el("tr", row.note ? "note" : `step ${row.status ?? ""}`);
      if (row.error) line.classList.add("error");
      const time = `+${(row.at / 1000).toFixed(2)}s`;
      const cells = row.note
        ? [time, row.note, "", "", ""]
        : [
            time,
            row.title,
            row.tool,
            row.error ??
              row.result ??
              (row.status === "incomplete" ? "no recorded outcome" : row.status) ??
              "",
            row.duration === undefined ? "…" : `${row.duration} ms`,
          ];
      line.append(...cells.map((text) => el("td", null, text)));
      if (row.item) {
        line.dataset.item = row.item;
        line.classList.toggle("selected", row.item === focus.selected);
        line.onclick = () => selectStatementKey(row.item, "both");
        line.ondblclick = () => openItem(row.item);
      }
      return line;
    }),
  );
}
function showTrace(shown) {
  $("trace").hidden = !shown;
  $("history-button").setAttribute("aria-pressed", String(shown));
  document.querySelector(".canvas-area").classList.toggle("tracing", shown);
}
// The scrubber steps through the statements of the run's verified path.
function pathStatements() {
  if (runPath?.state !== "verified") return [];
  return runPath.steps.flatMap((step, index) =>
    step.kind === "statement" ? [index] : [],
  );
}
function renderScrubber() {
  const positions = pathStatements();
  const input = $("replay");
  input.disabled = running || !positions.length;
  input.max = String(Math.max(0, positions.length - 1));
  if (!replay) input.value = input.max;
  $("replay-back").disabled = input.disabled || Number(input.value) <= 0;
  $("replay-forward").disabled = input.disabled || !replay;
  $("replay-label").textContent =
    runPath?.state === "unavailable"
      ? `No replay: ${runPath.reason}`
      : !positions.length
        ? running
          ? "Replay opens when the run ends"
          : ""
        : replay
          ? `Step ${Number(input.value) + 1} of ${positions.length}`
          : `${positions.length} steps`;
}
// Show the run as it stood at statement `position` of its path; the last
// position is the run's end.
function scrub(position) {
  const positions = pathStatements();
  if (!positions.length) return;
  const at = Math.max(0, Math.min(positions.length - 1, position));
  $("replay").value = String(at);
  replay =
    at < positions.length - 1
      ? replayAt(runPath.steps, positions[at])
      : undefined;
  renderScrubber();
  renderCanvas();
  const statement = replay && statementOfItem(replay.active);
  if (statement) revealStatement(statement);
}
// Keeps the node at `site` in view while the user has not panned since the
// run started: a tool call while it runs, or any step while it waits.
// Short internal steps (library calls, yields) are not followed.
function follow(site, status) {
  const statement = statementAt(site);
  if (statement && (status === "waiting" || effectOf(workflow, statement)))
    revealStatement(statement);
}
async function run() {
  resetRun();
  running = true;
  runStarted = performance.now();
  following = true;
  showTrace(true);
  controls();
  renderInvocationOutput();
  if (selected) renderCallData(selected);
  renderCanvas();
  $("run-status").textContent = "Running";
  const version = workflow.version;
  const startScope = activeScope;
  let sawTerminal = false;
  let observing = false;
  function observe(event) {
    if (event.workflowVersion !== version) {
      traceNote("Ignored an event for a different workflow revision.", true);
      renderTrace();
      return;
    }
    if (event.reset) {
      statuses.clear();
      errors.clear();
      calls.clear();
      traceRows = [];
      finalOutput = undefined;
      observationGap = true;
      sawTerminal = false;
      approvals.reset();
      $("run-status").textContent = "Recovering observation";
      traceNote(
        event.error ??
          "Observation gap: live evidence was discarded; retained outcomes follow.",
      );
      const display = event.display ?? {};
      for (const message of display.messages ?? []) traceNote(message);
      for (const [name, value] of Object.entries(display.statuses ?? {}))
        traceNote(`${name}: ${value}`);
      for (const [name, items] of Object.entries(display.lists ?? {}))
        traceNote(`${name}: ${items.join(", ")}`);
      for (const [name, value] of Object.entries(display.lights ?? {}))
        traceNote(`${name}: ${value}`);
      if (display.progress !== undefined)
        traceNote(`Progress ${display.progress}%`);
      renderInvocationOutput();
      if (selected) renderCallData(selected);
    }
    if (event.coverage) runCounts = counts(event.coverage, event.nodeCoverage);
    if (event.path) runPath = event.path;
    if (event.output !== undefined) {
      finalOutput = event.output;
      renderInvocationOutput();
    }
    if (event.site && !event.callOnly) {
      statuses.set(key(event.site), event.status);
      if (event.error) errors.set(key(event.site), event.error);
      else errors.delete(key(event.site));
      traceStatus(event);
    }
    if (event.site && event.callId) {
      const site = key(event.site);
      const occurrences = calls.get(site) ?? new Map();
      occurrences.set(event.callId, event);
      calls.set(site, occurrences);
      traceCall(event);
      if (selected?.actions?.some((a) => key(a) === site))
        renderCallData(selected);
    }
    if (!event.site && !event.reset && !event.callOnly) {
      $("run-status").textContent = event.status;
      if (event.status === "succeeded" || event.status === "failed")
        sawTerminal = true;
      traceNote(
        `Run ${event.status}${event.error ? `: ${event.error}` : ""}`,
        event.status === "failed",
      );
    }
    const delta = event.displayDelta ?? {};
    for (const message of delta.messagesAppended ?? []) traceNote(message);
    for (const [name, value] of Object.entries(delta.statuses ?? {}))
      traceNote(`${name}: ${value}`);
    for (const [name, items] of Object.entries(delta.listItemsAppended ?? {}))
      traceNote(`${name}: ${items.join(", ")}`);
    for (const [name, value] of Object.entries(delta.lights ?? {}))
      traceNote(`${name}: ${value}`);
    if (delta.progress !== undefined) traceNote(`Progress ${delta.progress}%`);
    if (event.approvalKey) approvals.add(event.approvalKey);
    renderCanvas();
    if (
      following &&
      event.site &&
      ["started", "waiting"].includes(event.status) &&
      !event.callOnly
    )
      follow(event.site, event.status);
    renderTrace();
  }
  try {
    const response = await fetch("/run", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ version, inputs: runInputs }),
    });
    if (!response.ok) throw (await response.json()).error;
    observing = true;
    const reader = response.body
      .pipeThrough(new TextDecoderStream())
      .getReader();
    let pending = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      pending += value;
      pending = pending.replace(/\r\n/g, "\n");
      let end;
      while ((end = pending.indexOf("\n\n")) >= 0) {
        const frame = pending.slice(0, end);
        pending = pending.slice(end + 2);
        const type = frame
          .split("\n")
          .find((l) => l.startsWith("event:"))
          ?.slice(6)
          .trim();
        const data = frame
          .split("\n")
          .filter((l) => l.startsWith("data:"))
          .map((l) => l.slice(5).trimStart())
          .join("\n");
        if (type === "run_error") throw Error(data);
        if (type === "run_event" && data) observe(JSON.parse(data));
      }
    }
    if (!sawTerminal) {
      $("run-status").textContent = "Disconnected";
      traceNote(
        "Observation ended before a terminal event. The process may still be running.",
        true,
      );
    }
  } catch (error) {
    if (!sawTerminal)
      $("run-status").textContent = observing
        ? "Observation unavailable"
        : "Could not start";
    traceNote(errorText(error), true);
  } finally {
    running = false;
    runEnded = true;
    // A run followed to its end returns to the scope it started from.
    if (following && workflow.version === version && key(activeScope) !== key(startScope)) {
      activeScope = startScope;
      renderScopes();
    }
    following = false;
    renderInvocationOutput();
    if (selected) renderCallData(selected);
    approvals.reset();
    controls();
    renderScrubber();
    renderCanvas();
    renderTrace();
  }
}
const svgIcon = (content) =>
  `<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg">${content}</svg>`;
const icons = {
  start: svgIcon(
    '<path d="M27 3 12 27h12l-3 18 16-26H25l2-16Z" fill="#e76b52"/>',
  ),
  gmail: svgIcon(
    '<path d="M6 13v27" stroke="#4285f4" stroke-width="8"/><path d="M42 13v27" stroke="#34a853" stroke-width="8"/><path d="m6 12 18 14 18-14" stroke="#ea4335" stroke-width="8" stroke-linejoin="round"/><path d="m6 12 7 5" stroke="#c5221f" stroke-width="8"/><path d="m35 17 7-5" stroke="#fbbc04" stroke-width="8"/>',
  ),
  message: svgIcon(
    '<path d="M9 8h30a4 4 0 0 1 4 4v21a4 4 0 0 1-4 4H22L11 44v-7H9a4 4 0 0 1-4-4V12a4 4 0 0 1 4-4Z" stroke="#657bc0" stroke-width="3"/><path d="M14 18h20M14 26h13" stroke="#657bc0" stroke-width="3" stroke-linecap="round"/>',
  ),
  status: svgIcon(
    '<rect x="5" y="5" width="38" height="38" rx="10" stroke="#cf9b41" stroke-width="3"/><path d="m14 24 7 7 14-15" stroke="#cf9b41" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/>',
  ),
  web: svgIcon(
    '<circle cx="24" cy="24" r="19" stroke="#4c83ca" stroke-width="3"/><ellipse cx="24" cy="24" rx="8" ry="19" stroke="#4c83ca" stroke-width="3"/><path d="M6 18h36M6 30h36" stroke="#4c83ca" stroke-width="3"/>',
  ),
  branch: svgIcon(
    '<path d="M7 24h10c9 0 5-14 15-14h8M17 24c9 0 5 14 15 14h8" stroke="#69a184" stroke-width="3"/><circle cx="7" cy="24" r="4" fill="white" stroke="#69a184" stroke-width="3"/><path d="m35 5 6 5-6 5m0 18 6 5-6 5" stroke="#69a184" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"/>',
  ),
};
const catalog = {
  "research.search": {
    name: "Find sources",
    app: "Research",
    icon: "web",
    fields: [
      ["topic", "Research topic", "text"],
      ["max_sources", "Maximum sources", "number", { min: 1, max: 3 }],
    ],
  },
  "research.synthesize": {
    name: "Write brief",
    app: "Research",
    icon: "message",
  },
  "metrics.fetch": {
    name: "Get metrics",
    app: "Analytics",
    icon: "status",
    fields: [
      ["workspace", "Workspace", "text"],
      ["period", "Period", "select", { options: ["current", "previous"] }],
    ],
  },
  "metrics.report": { name: "Compare KPIs", app: "Analytics", icon: "branch" },
  "display.show_message": {
    name: "Show message",
    app: "Display",
    icon: "message",
    description: "Display a message when this step runs",
    fields: [["text", "Message", "text"]],
  },
  "display.set_status": {
    name: "Update status",
    app: "Display",
    icon: "status",
    description: "Set a named status for this workflow",
    fields: [
      ["key", "Status name", "text"],
      ["value", "Status value", "text"],
    ],
  },
  "gmail.list_recent": {
    name: "Get recent emails",
    app: "Gmail",
    icon: "gmail",
    description: "Read sample emails from your inbox",
    fields: [["count", "Number of emails", "number"]],
  },
  "web.search": {
    name: "Search the web",
    app: "Web",
    icon: "web",
    description: "Search with a local mock response",
    fields: [["query", "Search query", "text"]],
  },
};
let zoom = 1,
  graphWidth = 1200,
  graphHeight = 700,
  visualNodes = [],
  wireItems = new Map(),
  insertAt,
  activeScope;
function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}
function icon(name) {
  const node = el("span", "node-icon");
  node.innerHTML = icons[name] ?? icons.message;
  return node;
}
function recordInput(s) {
  return canonicalRecordInput(workflow, s);
}
// The canvas's names for what opens a block.
const SCOPE_LABELS = {
  closure: "Function body",
  then: "Branch",
  else: "Branch",
  for: "Loop body",
  while: "Loop body",
  try: "Try block",
  catch: "Catch block",
  finally: "Finally block",
};
function canvasScopes() {
  const view = viewOf(workflow);
  return [
    ...view.callables.map((callable) => view.scopes.find((scope) => key(scope.block) === key(callable.block))),
    ...view.nodes.filter((node) => node.kind === "control").flatMap((node) => [
      ...node.arms.map((arm, position) => ({ block: arm.block, kind: arm.role, statement: node.statement,
        caption: armCaption(workflow, view.nodes.indexOf(node), position) })),
      ...(node.header?.actions.length ? [{ block: node.header.expression, kind: node.header.role, statement: node.statement }] : []),
    ]),
  ];
}

function renderScopes() {
  $("scope").replaceChildren(...canvasScopes().map((scope, index) => {
    const option = el("option", null, scope.kind === "entry" ? (workflowTitle(workflow)?.title ?? "Workflow entry")
      : scope.caption ? scope.caption.title
      : ["condition", "iterable"].includes(scope.kind) ? `${scope.kind} actions ${index}` : `${SCOPE_LABELS[scope.kind]} ${index}`);
    option.value = String(index); option.selected = key(scope.block) === key(activeScope);
    return option;
  }));
}

// A node's wording: lash's caption for it when it has one, over this host's
// deterministic wording, which then names what the step does underneath.
function model(s) {
  const details = describe(s);
  const carrier = viewOf(workflow).nodes.findIndex((node) => key(node.site) === key(s.site));
  const captions = nodeCaptions(workflow, carrier);
  if (!captions.length) return { ...details, captions };
  return {
    ...details,
    captions,
    title: captions[0].caption.title,
    subtitle: captions.length > 1 ? captions.slice(1).map((item) => item.caption.title).join(" · ") : details.title,
    description: captions[0].caption.description,
    review: captions.some((item) => item.review),
  };
}
// The printed line a node's readable range starts on.
function printedLine(carrier) {
  const first = carrier?.readable?.[0]?.first;
  return first && workflow.source?.split("\n")[first - 1];
}
function describe(s) {
  if (s.node_kind === "code") {
    const carrier = viewOf(workflow).nodes.find((node) => node.kind === "code" && key(node.site) === key(s.site));
    const range = carrier.canonical;
    return { s, info: { name: "Code", app: "Code", icon: "status" },
      title: "Code", subtitle: `Kernel lines ${range.first}–${range.last} · ${carrier.members.length} statements`, generic: true };
  }
  const effect = effectOf(workflow, s);
  if (effect) {
    const info = catalog[effect] ?? {
      name: effect,
      app: "Action",
      icon: "message",
    };
    const record = recordInput(s);
    const preview = record?.record
      .map((f) => literal(f.value))
      .filter((v) => v !== undefined)
      .join(" · ");
    return {
      s,
      info,
      title: info.app + " · " + info.name,
      subtitle: preview || info.name,
    };
  }
  if (s.statement.if) {
    const carrier = viewOf(workflow).nodes.find((node) => key(node.site) === key(s.site));
    const condition = conditionText(printedLine(carrier));
    return {
      s,
      info: { name: "Condition", app: "Flow", icon: "branch" },
      title: condition ? `If ${condition}` : "If · Choose a path",
      subtitle: "True or false",
    };
  }
  const kind =
    actionOf(workflow, s)?.kind ??
    (typeof s.statement === "string" ? s.statement : Object.keys(s.statement)[0]);
  const names = {
    call: "Call function",
    spawn: "Start task",
    try: "Try / catch",
    for: "For each",
    while: "While",
    join: "Wait for task",
    join_many: "Wait for tasks",
    cancel: "Cancel task",
    sleep: "Wait",
    yield: "Yield",
  };
  const library = actionOf(workflow, s)?.callee?.library;
  const name = library ? (workflow.functionLabels[library.name] ?? library.name) : (names[kind] ?? kind);
  return {
    s,
    info: { name, app: "Flow", icon: "status" },
    title: name,
    subtitle: library ? `${library.name} · ${library.id}` : "Open to inspect",
    generic: true,
  };
}
function project() {
  dataView = buildDataflow(workflow, activeScope);
  function decorate(nodes) {
    return nodes.map((node) => {
      const details = model(node.s);
      if (node.kind === "value")
        Object.assign(details, {
          info: {
            name: node.binding.name,
            app: node.binding.assigned ? "Variable" : "Value",
            icon: "status",
          },
          title: node.binding.name,
          subtitle: node.binding.assigned ? "Mutable binding" : "Named value",
          generic: false,
        });
      if (node.kind === "output")
        Object.assign(details, {
          info: {
            name: node.terminal === "finish" ? "Finish" : "Return",
            app: "Result",
            icon: "message",
          },
          title: node.terminal === "finish" ? "Finish" : "Return",
          subtitle:
            node.terminal === "finish"
              ? "End the whole invocation"
              : node.workflowOutput
                ? "Return the workflow result"
                : "Return to the caller",
          generic: false,
        });
      if (node.kind === "write")
        Object.assign(details, {
          info: { name: "Set value", app: "Variable", icon: "status" },
          title: "Set value",
          subtitle: node.outputs[0]?.name,
          generic: false,
        });
      return {
        ...details,
        ...node,
        ...(node.children ? { children: decorateBranches(node.children) } : {}),
      };
    });
  }
  const decorateBranches = (branches) => branches.map(decorate);
  return decorate(dataView.nodes);
}
function positionAfter(s) {
  const view = viewOf(workflow);
  const carrier = view.nodes.find((node) => key(node.site) === key(s.site));
  const last = carrier?.kind === "code"
    ? carrier.members.filter((site) => view.statements.some((row) => key(row.site) === key(site) && key(row.block) === key(s.block))).at(-1)
    : s.statement_site;
  const index = view.statements.findIndex((row) => key(row.site) === key(last ?? s.site));
  const next = view.statements
    .slice(index + 1)
    .find((t) => key(t.block) === key(s.block));
  return { block: s.block, ...(next ? { before: next.site } : {}) };
}
function startPosition() {
  return activeScope ? scopeStart(workflow, activeScope) : undefined;
}
function addPort(x, y, at, label) {
  if (!at) return;
  const button = el("button", "add-port", "+");
  button.style.left = x + "px";
  button.style.top = y - 12 + "px";
  button.setAttribute("aria-label", label);
  button.disabled = busy || running;
  button.onclick = () => openCatalog(at);
  $("flow").append(button);
}
const cardWidth = 230;
function nodeHeight(node) {
  return (
    74 +
    (node.inputs.length + node.outputs.length) * 27 +
    (node.inputs.length ? 21 : 0) +
    (node.outputs.length ? 21 : 0) +
    (node.kind === "control" ? (node.carrier.arms.length + Number((node.carrier.header?.actions.length ?? 0) > 0)) * 30 : 0)
  );
}
// A canvas item's key: its statement's site key, `node:<kind>` for a node
// with no statement, or a wire's `wire:` key.
const itemOf = (node) => (node.s ? key(node.s.site) : `node:${node.kind}`);
// What a node shows of the run: the tool's result, the error it failed
// with, or the workflow's return value on its Return or Finish node.
function runResult(node, shown) {
  if (shown.state === "failed" && shown.error) return shown.error;
  const occurrences = node.kind === "action" ? calls.get(key(node.site)) : undefined;
  const last = occurrences && [...occurrences.values()].at(-1);
  if (last?.call) return chip(last.call.output);
  if (node.kind === "output" && node.workflowOutput && finalOutput !== undefined)
    return `→ ${chip(finalOutput)}`;
}
const STATE_GLYPHS = {
  done: "✓",
  failed: "!",
  incomplete: "?",
  active: "…",
  waiting: "…",
};
function drawNode(node, x, y) {
  const card = el("section", "data-node " + node.kind);
  card.style.left = x + "px";
  card.style.top = y + "px";
  const item = itemOf(node);
  card.dataset.item = item;
  const shown = node.s ? nodeState(node.s, runView()) : {};
  const state = shown.state;
  if (state) {
    card.classList.add(`run-${state}`);
    if (state === "incomplete")
      card.title = "Incomplete evidence: this step's outcome was not observed.";
  }
  card.ondblclick = () => openItem(item);
  const header = el("button", "data-node-header");
  header.setAttribute("aria-label", node.title);
  header.append(icon(node.info.icon));
  const heading = el("span");
  heading.append(
    el("strong", null, node.title),
    el("small", null, node.subtitle),
  );
  header.append(heading);
  if (node.description) header.title = node.description;
  if (node.review) {
    const badge = el("span", "caption-review", "Review");
    badge.title = "An edit changed this step since its caption was written. Open it to check the caption.";
    header.append(badge);
  }
  if (shown.runs > 0)
    header.append(el("span", "run-count", `×${shown.runs}`));
  if (STATE_GLYPHS[state])
    header.append(el("span", "data-status", STATE_GLYPHS[state]));
  header.onclick = () => selectItem(item, "code");
  const opener = el("button", "node-open", "Open");
  opener.setAttribute("aria-label", `Open ${node.title}`);
  opener.onclick = () => openItem(item);
  card.append(header, opener);
  const result = runResult(node, shown);
  if (result)
    card.append(el("div", `result-chip${state === "failed" ? " failed" : ""}`, result));
  const placed = { ...node, x, y, inputPoints: [], outputPoints: [] };
  let row = 74;
  for (const [direction, ports] of [
    ["input", node.inputs],
    ["output", node.outputs],
  ]) {
    if (!ports.length) continue;
    card.append(
      el("div", "port-heading", direction === "input" ? "INPUTS" : "OUTPUTS"),
    );
    row += 21;
    for (const port of ports) {
      const button = el(
        "button",
        "data-port " + direction + (port.discarded ? " discarded" : ""),
      );
      button.setAttribute(
        "aria-label",
        `${direction === "input" ? "Configure" : "Connect"} ${node.title} ${port.name}`,
      );
      button.title =
        direction === "input"
          ? `${port.name}: ${typeName(port.type)} ← ${expressionName(port.expression)}`
          : `${port.name}: ${typeName(port.type)}`;
      button.append(el("i", "socket"), el("span", "port-name", port.name));
      const fixed =
        direction === "input"
          ? constantValue(port.expression)
          : { known: false };
      if (fixed.known)
        button.append(el("span", "port-literal", JSON.stringify(fixed.value)));
      button.append(el("span", "port-type", typeName(port.type)));
      const point = {
        x: direction === "input" ? x : x + cardWidth,
        y: y + row + 13,
        port,
        node: placed,
      };
      placed[direction + "Points"].push(point);
      // A click on an input selects its node, or completes a connection;
      // a double-click opens its editor. A click on an output starts a
      // connection from it.
      button.ondblclick = (event) => {
        event.stopPropagation();
        if (direction === "input" && port.target) openPortOf(node, port);
        else openItem(item);
      };
      button.onclick = () => {
        if (direction === "input") {
          if (connecting && port.target) connectPort(port, connecting);
          else selectItem(item, "code");
        } else if (port.discarded || port.stored) selectItem(item, "code");
        else {
          connecting = port;
          setFocus(select(close(focus), item), "code");
          report(
            `Connect ${port.name}: choose an input port, or press Escape to cancel.`,
          );
          document
            .querySelectorAll(".data-port.output")
            .forEach((p) => p.classList.remove("chosen"));
          button.classList.add("chosen");
        }
      };
      if (direction === "output" && port.discarded)
        button.title += " · result is discarded by this statement";
      card.append(button);
      row += 27;
    }
  }
  if (node.kind === "control") {
    const index = viewOf(workflow).nodes.indexOf(node.carrier);
    for (const [position, arm] of node.carrier.arms.entries()) {
      const entries = runCounts.scopes.get(key(arm.block));
      const name = armCaption(workflow, index, position)?.title ?? arm.role;
      const button = el("button", "control-arm", `${name}${entries === undefined ? "" : ` · ${entries} entries`}`);
      button.onclick = () => { activeScope = arm.block; renderScopes(); renderCanvas(); fit(); };
      card.append(button);
    }
    if (node.carrier.header?.actions.length) {
      const button = el("button", "control-arm", "Inspect header actions");
      button.onclick = () => { activeScope = node.carrier.header.expression; renderScopes(); renderCanvas(); fit(); };
      card.append(button);
    }
  }
  $("flow").append(card);
  visualNodes.push(placed);
  if (node.s && !node.branch && node.kind !== "output")
    addPort(
      x + cardWidth + 40,
      y + 22,
      positionAfter(node.s),
      `Add step after ${node.title}`,
    );
  return placed;
}
function controlWire(from, to, label) {
  const ns = "http://www.w3.org/2000/svg";
  const path = document.createElementNS(ns, "path");
  const a = { x: from.x + cardWidth, y: from.y + 22 },
    b = { x: to.x, y: to.y + 22 };
  path.setAttribute(
    "d",
    `M${a.x},${a.y} C${a.x + 50},${a.y} ${b.x - 50},${b.y} ${b.x},${b.y}`,
  );
  path.setAttribute("class", "control-wire");
  path.setAttribute("marker-end", "url(#arrow)");
  // A branch's edge shows whether the run took that arm.
  if (label && from.s) {
    const arm = armsTaken(viewOf(workflow), runView(), from.s.site)[
      label === "True" ? "then" : "else"
    ];
    if (arm !== undefined) path.classList.add(arm ? "taken" : "untaken");
  }
  $("wires").append(path);
  if (label) {
    const text = document.createElementNS(ns, "text");
    text.setAttribute("x", a.x + 38);
    text.setAttribute("y", a.y - 9);
    text.setAttribute("class", "wire-label");
    text.textContent = label;
    $("wires").append(text);
  }
}
function branchHeight(nodes) {
  return Math.max(
    350,
    ...nodes.map((n) =>
      n.branch
        ? branchHeight(n.children[0]) + branchHeight(n.children[1])
        : nodeHeight(n) + 70,
    ),
  );
}
function layout(nodes, x, y, incoming) {
  let ends = incoming;
  for (const node of nodes) {
    const placed = drawNode(node, x, y);
    for (const prior of ends) {
      if (!node.carrier?.lazy.length && !prior.carrier?.lazy.length) controlWire(prior, placed, prior.label);
    }
    if (node.branch) {
      const top = y - branchHeight(node.children[0]) / 2,
        bottom = y + branchHeight(node.children[1]) / 2;
      const outputs = [];
      for (const [i, branch] of node.children.entries()) {
        if (branch.length)
          outputs.push(
            ...layout(branch, x + 300, i ? bottom : top, [
              { ...placed, label: i ? "False" : "True" },
            ]),
          );
        else outputs.push({ ...placed, label: i ? "False" : "True" });
      }
      if (!outputs.length) return [];
      ends = outputs;
      x = Math.max(...outputs.map((p) => p.x)) + 300;
    } else {
      if (node.carrier?.falls_through === false) { ends = []; x += 300; continue; }
      ends = [placed];
      x += 300;
    }
  }
  return ends;
}
function dataWires() {
  const ns = "http://www.w3.org/2000/svg";
  for (const node of visualNodes)
    for (const point of node.inputPoints) {
      for (const id of point.port.references ?? []) {
        const sourceNode = visualNodes.find((n) =>
          n.outputs.some((p) => p.binding === id),
        );
        if (!sourceNode) continue;
        let path = [];
        let expression = point.port.expression;
        while (expression?.member?.field) {
          path.unshift(expression.member.field.field);
          expression = expression.member.field.target;
        }
        const source =
          sourceNode.outputPoints.find(
            (p) =>
              p.port.binding === id &&
              JSON.stringify(p.port.path ?? []) === JSON.stringify(path),
          ) ?? sourceNode.outputPoints.find((p) => p.port.binding === id);
        if (!source) continue;
        const edge = document.createElementNS(ns, "path");
        const lift = Math.min(source.y, point.y) - 45;
        const d =
          point.x > source.x
            ? `M${source.x},${source.y} C${source.x + 90},${source.y} ${point.x - 90},${point.y} ${point.x},${point.y}`
            : `M${source.x},${source.y} C${source.x + 60},${lift} ${point.x - 60},${lift} ${point.x},${point.y}`;
        edge.setAttribute("d", d);
        edge.setAttribute("class", "data-wire");
        edge.setAttribute("marker-end", "url(#data-arrow)");
        // A wider transparent stroke takes the wire's clicks: a click
        // selects the connection and a double-click opens its input.
        const hit = document.createElementNS(ns, "path");
        hit.setAttribute("d", d);
        hit.setAttribute("class", "wire-hit");
        const item = `wire:${id}>${itemOf(node)}>${point.port.name}`;
        wireItems.set(item, { node, port: point.port });
        const group = document.createElementNS(ns, "g");
        group.setAttribute("class", "wire");
        group.dataset.item = item;
        const title = document.createElementNS(ns, "title");
        title.textContent = `${sourceNode.title}.${source.port.name} → ${node.title}.${point.port.name}`;
        group.append(title, edge, hit);
        group.addEventListener("click", () => selectItem(item, "code"));
        group.addEventListener("dblclick", () => openItem(item));
        $("wires").append(group);
      }
    }
}
function renderCanvas() {
  if (!workflow) return;
  visualNodes = [];
  wireItems = new Map();
  $("flow").replaceChildren();
  $("wires").innerHTML =
    '<defs><marker id="arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="5" markerHeight="5" orient="auto"><path d="M0 0 8 4 0 8Z" fill="#aeb5bc"/></marker><marker id="data-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="5" markerHeight="5" orient="auto"><path d="M0 0 8 4 0 8Z" fill="#6287b1"/></marker></defs>';
  const roots = project(),
    center = Math.max(160, branchHeight(roots) / 2);
  const rootScope = !activeScope?.path?.length;
  const used = new Set(),
    produced = new Set();
  function collect(nodes) {
    for (const node of nodes) {
      if (node.binding) produced.add(node.binding.id);
      node.inputs.forEach((port) =>
        port.references.forEach((id) => used.add(id)),
      );
      if (node.children) node.children.forEach(collect);
    }
  }
  collect(roots);
  const scopeBindings = rootScope
    ? dataView.parameters
    : viewOf(workflow).dataflow.bindings.filter(
        (binding) =>
          (key(binding.block) === key(activeScope) && binding.kind !== "let") ||
          (used.has(binding.id) && !produced.has(binding.id)),
      );
  const start = drawNode(
    {
      kind: rootScope ? "input" : "scope-input",
      info: {
        name: rootScope ? "Workflow Input" : "Scope inputs",
        app: "Invocation",
        icon: "start",
      },
      title: rootScope ? "Workflow Input" : "Scope inputs",
      subtitle: rootScope
        ? "Parameters for this invocation"
        : "Bindings in this scope",
      inputs: [],
      outputs: scopeBindings.flatMap((b) => dataView.outputPorts(b)),
    },
    70,
    center,
  );
  if (!start.outputs.length) start.subtitle = "No parameters";
  addPort(
    70 + cardWidth + 40,
    center + 22,
    startPosition(),
    "Add step after Workflow Input",
  );
  layout(roots, 370, center, [start]);
  dataWires();
  graphWidth = Math.max(900, ...visualNodes.map((n) => n.x + cardWidth + 70));
  graphHeight = Math.max(
    650,
    ...visualNodes.map((n) => n.y + nodeHeight(n) + 100),
  );
  $("scene").style.width = graphWidth + "px";
  $("scene").style.height = graphHeight + "px";
  setZoom(zoom);
  $("node-count").textContent =
    `${viewOf(workflow).nodes.length} coarse nodes · ${viewOf(workflow).statements.length} statements · ${viewOf(workflow).callables.length} callables`;
  const title = workflowTitle(workflow);
  document.querySelector("h1").textContent = title?.title ?? workflow.entry.replaceAll("_", " ");
  document.querySelector("h1").title = title?.description ?? "";
  applySelection();
}
function panel(title) {
  $("node-panel").hidden = false;
  $("panel-kicker").textContent = title;
  $("node-form").replaceChildren();
  return $("node-form");
}
function identity(parent, info) {
  const row = el("div", "form-identity");
  row.append(icon(info.icon));
  const text = el("div");
  text.append(el("h2", null, info.name), el("p", null, info.app));
  row.append(text);
  parent.append(row);
}
function fields(parent, info, values) {
  const inputs = {};
  for (const [name, label, type, config = {}] of info.fields ?? []) {
    const id = "field-" + name;
    const caption = el("label", null, label);
    caption.htmlFor = id;
    const input = el(
      type === "select"
        ? "select"
        : type === "text" && name === "text"
          ? "textarea"
          : "input",
    );
    for (const value of config.options ?? []) {
      const option = el("option", null, value);
      option.value = value;
      input.append(option);
    }
    input.id = id;
    if (input.tagName === "INPUT") input.type = type;
    input.value = values[name] ?? "";
    if (type === "number") {
      input.min = String(config.min ?? 1);
      input.max = String(config.max ?? 100);
    }
    parent.append(caption, input);
    inputs[name] = input;
  }
  return () =>
    Object.fromEntries(
      Object.entries(inputs).map(([name, input]) => {
        if (input.type === "number") {
          const value = Number(input.value);
          if (
            !Number.isInteger(value) ||
            value < Number(input.min) ||
            value > Number(input.max)
          )
            throw Error(
              `Choose a number between ${input.min} and ${input.max}.`,
            );
          return [name, value];
        }
        return [name, input.value];
      }),
    );
}
async function applyVisual(changes) {
  const planned = await call("/workflow/plan", {
    version: workflow.version,
    operations: changes,
  });
  const answer = await call("/workflow/edits", {
    version: planned.version,
    edits: planned.transaction.edits,
  });
  $("node-panel").hidden = true;
  show(answer.workflow, answer.correspondence);
  report("Changes saved");
  setTimeout(() => {
    if ($("result").textContent === "Changes saved") report("");
  }, 3000);
}
function openCatalog(at) {
  if (busy || running) return;
  setFocus(open(focus));
  insertAt = at ?? startPosition();
  const container = panel("ADD A STEP");
  container.append(
    el("h2", null, "What happens next?"),
    el("p", "help", "Choose an action to connect to your workflow."),
  );
  for (const [effect, info] of Object.entries(catalog)) {
    if (!info.fields) continue;
    const button = el("button", "catalog-item");
    button.append(icon(info.icon));
    const text = el("span");
    text.append(el("strong", null, info.app), el("small", null, info.name));
    button.append(text);
    button.onclick = () => newNode(effect);
    container.append(button);
  }
}
function newNode(effect) {
  const container = panel("NEW STEP");
  const back = el("button", "quiet back", "← All actions");
  back.onclick = () => openCatalog(insertAt);
  container.append(back);
  const info = catalog[effect];
  identity(container, info);
  container.append(el("p", "help", info.description));
  const read = fields(container, info, presets[effect]);
  const save = el("button", "primary save-node", "Add to workflow");
  save.onclick = () =>
    action(async () => {
      const values = read();
      await applyVisual([
        {
          insert_step: {
            base: workflow.identity,
            at: insertAt,
            effect,
            args: [expression(values)],
            result: "any",
            binding: availableName(workflow, "step"),
          },
        },
      ]);
    });
  container.append(
    save,
    el("p", "help", "Uses a mock connection. No external requests."),
  );
}
function dataTree(value, depth = 0) {
  if (value === null || typeof value !== "object")
    return el("span", "data-value", value === null ? "null" : String(value));
  const group = el("div", "data-tree");
  const entries = Object.entries(value);
  if (!entries.length)
    return el(
      "span",
      "data-value",
      Array.isArray(value) ? "Empty list" : "Empty object",
    );
  for (const [name, child] of entries) {
    const row = el("div", "data-row");
    if (child !== null && typeof child === "object") {
      const details = el("details");
      details.open = depth < 1;
      details.append(
        el(
          "summary",
          null,
          `${name} · ${Array.isArray(child) ? `${child.length} items` : "Object"}`,
        ),
        dataTree(child, depth + 1),
      );
      row.append(details);
    } else row.append(el("span", "data-key", name), dataTree(child, depth + 1));
    group.append(row);
  }
  return group;
}
function renderCallData(statement) {
  const container = $("call-data");
  if (!container) return;
  container.replaceChildren(el("h3", null, "Execution data"));
  const sites = statement.actions?.length ? statement.actions : [statement.site];
  const records = sites.flatMap((site) => [
    ...(calls.get(key(site))?.values() ?? []),
  ]);
  if (!records.length) {
    container.append(
      el(
        "p",
        "help",
        observationGap
          ? "No call data is available in the replacement snapshot. Earlier live observations were discarded."
          : running
            ? "Waiting for this step to settle…"
            : "Run this workflow to inspect its inputs and outputs.",
      ),
    );
    return;
  }
  const select = el("select");
  select.setAttribute("aria-label", "Step occurrence");
  records.forEach((record, index) => {
    const option = el("option", null, `Call ${index + 1} · ${record.status}`);
    option.value = String(index);
    select.append(option);
  });
  select.value = String(records.length - 1);
  const body = el("div");
  function showRecord() {
    const record = records[Number(select.value)];
    body.replaceChildren(el("p", "help", `Call ${record.callId}`));
    if (!record.call) {
      body.append(
        el(
          "p",
          "help",
          "Call data is unavailable. It may be unfinished or not retained.",
        ),
      );
      return;
    }
    body.append(
      el("h4", null, "Input"),
      dataTree(record.call.input),
      el("h4", null, record.status === "failed" ? "Failure" : "Output"),
      dataTree(record.call.output),
    );
  }
  select.onchange = showRecord;
  container.append(select, body);
  showRecord();
}
function initialInputs(next) {
  // Mock invocation arguments for the examples' parameters, by name.
  const presets = {
    topic: "Reducing customer onboarding drop-off",
    workspace: "Acme Cloud",
    topics: [
      { title: "Release notes", urgent: true },
      { title: "Team offsite", urgent: false },
    ],
    items: [{ title: "Welcome aboard" }, { title: "Weekly summary" }],
  };
  return Object.fromEntries(
    (next.document.entries[next.entry]?.signature.params ?? []).map((param) => [
      param.name,
      presets[param.name] ?? defaultValue(param.ty),
    ]),
  );
}
function defaultValue(type) {
  if (type === "text") return "";
  if (type === "bool") return false;
  if (["int", "float", "number"].includes(type)) return 0;
  if (type?.enum) return type.enum[0];
  if (type?.list) return [];
  if (type?.record)
    return Object.fromEntries(
      fieldsOf(type)
        .filter((f) => !f.optional)
        .map((f) => [f.name, defaultValue(f.ty)]),
    );
  return null;
}
function constantControl(parent, label, type, value, id, config = {}) {
  const caption = el("label", null, label);
  caption.htmlFor = id;
  let input;
  if (type === "bool" || type?.enum) {
    input = el("select");
    const options = type === "bool" ? [true, false] : type.enum;
    options.forEach((v) => {
      const option = el("option", null, String(v));
      option.value = String(v);
      input.append(option);
    });
    input.value = String(value ?? options[0]);
  } else {
    const scalar = ["text", "int", "float", "number"].includes(type);
    input = el(scalar ? "input" : "textarea");
    if (scalar) input.type = type === "text" ? "text" : "number";
    if (input.type === "number") {
      input.step = type === "int" ? "1" : "any";
      if (config.min !== undefined) input.min = config.min;
      if (config.max !== undefined) input.max = config.max;
    }
    const initial = value ?? defaultValue(type);
    input.value = type === "text" ? initial : constantText(initial);
  }
  input.id = id;
  parent.append(caption, input);
  return () => {
    if (type === "bool") return input.value === "true";
    if (type === "text" || type?.enum) return input.value;
    if (["int", "float", "number"].includes(type)) {
      const result = Number(input.value);
      if (
        input.value.trim() === "" ||
        !Number.isFinite(result) ||
        (type === "int" && !Number.isSafeInteger(result))
      )
        throw Error(`Enter a valid ${typeName(type)} value.`);
      if (
        (config.min !== undefined && result < config.min) ||
        (config.max !== undefined && result > config.max)
      )
        throw Error(`Choose a number between ${config.min} and ${config.max}.`);
      return result;
    }
    try {
      const value = JSON.parse(input.value);
      if (
        type?.record &&
        (!value || typeof value !== "object" || Array.isArray(value))
      )
        throw Error("Expected an object.");
      if (type?.list && !Array.isArray(value)) throw Error("Expected a list.");
      if (type === "null" && value !== null) throw Error("Expected null.");
      return value;
    } catch {
      throw Error("Enter valid JSON for this structured value.");
    }
  };
}
function renderInvocationInputs(container) {
  const params = workflow.document.entries[workflow.entry]?.signature.params ?? [];
  if (!params.length) {
    container.append(
      el("p", "help", "This workflow takes no invocation parameters."),
    );
    return;
  }
  container.append(
    el(
      "p",
      "help",
      "These values are supplied when you run the workflow. They do not change the document.",
    ),
  );
  const reads = params.map((param, index) => [
    param.name,
    constantControl(
      container,
      `${param.name} · ${typeName(param.ty)}`,
      param.ty,
      runInputs[param.name],
      "invocation-" + index,
    ),
  ]);
  const save = el("button", "primary save-node", "Save inputs");
  save.onclick = () =>
    action(async () => {
      runInputs = Object.fromEntries(
        reads.map(([name, read]) => [name, read()]),
      );
      report("Invocation inputs saved");
    });
  container.append(save);
}
function replacePort(port, value) {
  return planInputEdit(workflow, port, value);
}
function connectPort(port, source) {
  if (busy || running) return;
  action(async () => {
    if (!(port.available ?? []).includes(source.binding))
      throw Error(
        "This value is not available at the selected input. Choose a value in scope that is defined before this step.",
      );
    await applyVisual(replacePort(port, source.expression));
    connecting = undefined;
    report(`Connected ${source.name} to ${port.name}`);
  });
}
function openPort(node, port) {
  if (busy) return;
  const container = panel(
    node.kind === "output" ? "RESULT MAPPING" : "EDIT INPUT",
  );
  identity(container, node.info);
  if (!port.target) {
    container.append(el("p", "help", "Edit this code in source, then re-import the document."));
    const source = el("button", null, "Edit source");
    source.onclick = () => $("source-editor").showModal();
    container.append(source);
    return;
  }
  container.append(el("h3", null, `${port.name} · ${typeName(port.type)}`));
  const modeLabel = el("label", null, "Value source");
  modeLabel.htmlFor = "mapping-mode";
  const mode = el("select");
  mode.id = "mapping-mode";
  for (const [value, label] of [
    ["constant", "Constant"],
    ["connection", "Connected value"],
  ]) {
    const option = el("option", null, label);
    option.value = value;
    mode.append(option);
  }
  const literal = constantValue(port.expression);
  const options = sourceOptions(dataView, port);
  const current = options.findIndex(
    (option) =>
      JSON.stringify(option.expression) === JSON.stringify(port.expression),
  );
  mode.value = literal.known
    ? "constant"
    : current >= 0
      ? "connection"
      : "constant";
  container.append(modeLabel, mode);
  const editor = el("div", "mapping-editor");
  container.append(editor);
  let read;
  function draw() {
    editor.replaceChildren();
    if (mode.value === "constant") {
      const hostConfig = node.info.fields?.find((f) => f[0] === port.name)?.[3];
      const holder = el("div");
      let currentType =
        port.expression?.literal?.float !== undefined &&
        ["number", "any"].includes(port.type)
          ? "float"
          : port.type;
      let value = literal.known ? literal.value : defaultValue(port.type);
      let original = literal.known ? port.expression : undefined;
      function drawConstant() {
        holder.replaceChildren();
        const readValue = constantControl(
          holder,
          "Constant value",
          currentType,
          value,
          "constant-value",
          hostConfig,
        );
        read = () => expression(readValue(), currentType, original);
      }
      if (port.acceptsType === "any" || port.type === "any") {
        const label = el("label", null, "Constant type");
        label.htmlFor = "constant-type";
        const select = el("select");
        select.id = "constant-type";
        const types = [
          "text",
          "int",
          "float",
          "number",
          "bool",
          "null",
          { record: { fields: [], rest: "any" } },
          { list: "any" },
          "any",
        ];
        types.forEach((type, index) => {
          const option = el("option", null, typeName(type));
          option.value = String(index);
          select.append(option);
        });
        const index = types.findIndex((type) =>
          typeof type === "string"
            ? type === currentType
            : !!type.record === !!currentType?.record &&
              !!type.list === !!currentType?.list,
        );
        select.value = String(index >= 0 ? index : types.length - 1);
        select.onchange = () => {
          currentType = types[Number(select.value)];
          value = defaultValue(currentType);
          original = undefined;
          drawConstant();
        };
        editor.append(label, select);
        editor.append(
          el(
            "p",
            "help",
            "The displayed type describes the current value. This binding has no declared type constraint.",
          ),
        );
      }
      editor.append(holder);
      drawConstant();
    } else if (mode.value === "connection") {
      const label = el("label", null, "Source output");
      label.htmlFor = "source-output";
      const select = el("select");
      select.id = "source-output";
      options.forEach((option, index) => {
        const item = el("option", null, option.label);
        item.value = String(index);
        select.append(item);
      });
      select.value = String(current >= 0 ? current : options.length ? 0 : -1);
      editor.append(
        label,
        select,
        el(
          "p",
          "help",
          "Only values visible at this input are selectable. Lash checks the connection when you save. Any means the type is not known statically.",
        ),
      );
      read = () => {
        const option = options[Number(select.value)];
        if (!option)
          throw Error(
            "No source is available. Define a value before this step.",
          );
        return option.expression;
      };
    }
  }
  mode.onchange = draw;
  draw();
  const save = el("button", "primary save-node", "Save mapping");
  save.onclick = () => action(() => applyVisual(replacePort(port, read())));
  container.append(save);
  renderCanvas();
  controls();
}

function renderInvocationOutput() {
  const container = $("invocation-output");
  if (!container) return;
  container.replaceChildren(
    el("h3", null, "Invocation result"),
    finalOutput === undefined
      ? el(
          "p",
          "help",
          running
            ? "Waiting for this invocation to finish…"
            : "No returned value recorded for this invocation.",
        )
      : dataTree(finalOutput),
  );
}
// A caption is changed by saving the whole new caption list; lash checks
// every site and the text. Saving one marks it reviewed.
async function saveCaption(site, caption) {
  if (caption && !caption.title) throw Error("Write a title for the caption.");
  const next = await call("/workflow/captions", {
    version: workflow.version,
    entries: withCaption(workflow.captions, site, caption),
    reviewed: [site],
  });
  show(next);
  report(caption ? "Caption saved" : "Caption removed");
}
let captionForms = 0;
function captionForm(container, heading, { site, caption, review }) {
  const form = el("section", "caption-form");
  form.append(el("h3", null, heading));
  if (review)
    form.append(el("p", "help caption-review-note",
      "An edit changed this step since its caption was written. Check that it still describes the step, then save it."));
  const id = `caption-${captionForms++}`;
  const title = el("input");
  title.id = `${id}-title`;
  title.value = caption?.title ?? "";
  const description = el("textarea");
  description.id = `${id}-description`;
  description.value = caption?.description ?? "";
  const titleLabel = el("label", null, "Title");
  titleLabel.htmlFor = title.id;
  const descriptionLabel = el("label", null, "Description (optional)");
  descriptionLabel.htmlFor = description.id;
  const save = el("button", "save-node", "Save caption");
  save.onclick = () => action(() => saveCaption(site, captionFrom(title.value, description.value)));
  form.append(titleLabel, title, descriptionLabel, description, save);
  if (caption) {
    const remove = el("button", "quiet save-node", "Remove caption");
    remove.onclick = () => action(() => saveCaption(site, undefined));
    form.append(remove);
  }
  container.append(form);
}
// A node's captions, or a form to add one where the node can carry it.
function nodeCaptionForms(container, node) {
  if (!node.carrier) return;
  if (node.captions?.length)
    for (const item of node.captions) captionForm(container, "Caption", item);
  else {
    const site = captionSite(node.carrier);
    if (site) captionForm(container, "Add a caption", { site });
  }
}
// The inspector of an open node. Opening goes through `openItem`.
function openNode(node) {
  const container = panel(
    node.kind === "input"
      ? "WORKFLOW INPUTS"
      : node.kind === "output"
        ? "TERMINAL STEP"
        : "NODE SETTINGS",
  );
  identity(container, node.info);
  nodeCaptionForms(container, node);
  if (node.kind === "input") {
    const site = { unit: { function: workflow.entry }, path: [] };
    captionForm(container, "Workflow title", {
      site,
      caption: workflowTitle(workflow),
      review: workflow.captionsToReview.some((item) => key(item) === key(site)),
    });
  }
  if (node.kind === "code") {
    const range = node.carrier.canonical;
    const source = viewOf(workflow).canonical_source.split("\n").slice(range.first - 1, range.last).join("\n");
    container.append(el("p", "help", "Edit this code in source. This group is entered once per first-statement start; individual lines may return or fail earlier."), el("pre", "code-fragment", source));
    controls();
    return;
  }
  if (node.kind === "input") renderInvocationInputs(container);
  else if (node.kind === "scope-input") {
    container.append(
      el(
        "p",
        "help",
        "These values come from the enclosing scope or this function’s caller. Run still invokes the workflow entry.",
      ),
    );
    for (const output of node.outputs)
      container.append(
        el("p", "mapping-row", `${output.name} · ${typeName(output.type)}`),
      );
  } else {
    for (const port of node.inputs) {
      const button = el("button", "mapping-button");
      button.append(
        el("strong", null, port.name),
        el("span", "port-type", typeName(port.type)),
        el("small", null, expressionName(port.expression)),
      );
      button.disabled = !port.target;
      button.onclick = () => { if (port.target) openPort(node, port); };
      container.append(button);
    }
    for (const port of node.wholeInputs ?? []) {
      const edit = el("button", null, `Edit ${port.name} object`);
      edit.onclick = () => openPort(node, port);
      container.append(edit);
    }
    if (node.outputs.some((port) => port.stored))
      container.append(
        el(
          "p",
          "help",
          "This node writes mutable state. Later reads use the variable’s value at that point in execution.",
        ),
      );
    if (node.kind === "output") {
      container.append(
        el(
          "p",
          "help",
          node.terminal === "finish"
            ? "Finish ends the whole invocation with this value, even inside a nested function. No later step runs."
            : node.workflowOutput
              ? "Return exits the entry function after enclosing finally blocks run. A finally block can change the result. Editing the result mapping leaves this step in place."
              : "Return exits this function after enclosing finally blocks run. A finally block can change the result. The caller can continue. Editing the result mapping leaves this step in place.",
        ),
      );
    }
    if (node.kind === "output" && !node.workflowOutput)
      container.append(
        el(
          "p",
          "help",
          "This returns to the enclosing function’s caller. Its result is not the workflow invocation result.",
        ),
      );
    if (node.binding) {
      if (node.binding.assigned)
        container.append(
          el(
            "p",
            "help",
            "This variable is reassigned. Data wires identify the binding; its value depends on execution order.",
          ),
        );
    }
    if (node.s?.actions?.length) {
      const data = el("section", "call-data");
      data.id = "call-data";
      container.append(data);
      renderCallData(node.s);
    }
    if (node.kind === "output" && node.workflowOutput !== false) {
      const result = el("section");
      result.id = "invocation-output";
      container.append(result);
      renderInvocationOutput();
    }
    if (node.generic || node.kind === "control") {
      container.append(
        el(
          "p",
          "help",
          "Nested blocks are available from the scope menu. Use source to change code outside the canvas operations.",
        ),
      );

    }
    if (node.s && node.kind === "action") {
      const remove = el("button", "danger save-node", "Delete node");
      remove.onclick = () =>
        action(() =>
          applyVisual([
            { remove_step: { base: workflow.identity, action: node.action?.site ?? actionOf(workflow, node.s)?.site } },
          ]),
        );
      container.append(remove);
    }
  }
  controls();
}
// The stage is the scene's scaled size padded by half a viewport on every
// side, so the canvas pans freely even when the graph fits: any edge of the
// graph can reach the middle of the view.
let pad = { x: 0, y: 0 };
function setZoom(value) {
  const viewport = $("viewport");
  zoom = clampZoom(value);
  const next = { x: viewport.clientWidth / 2, y: viewport.clientHeight / 2 };
  $("scene").style.transform = `translate(${next.x}px, ${next.y}px) scale(${zoom})`;
  $("stage").style.width = graphWidth * zoom + 2 * next.x + "px";
  $("stage").style.height = graphHeight * zoom + 2 * next.y + "px";
  // A resized view keeps the scene where it was.
  viewport.scrollLeft += next.x - pad.x;
  viewport.scrollTop += next.y - pad.y;
  pad = next;
  $("zoom-label").textContent = Math.round(zoom * 100) + "%";
}
// The view as zoom and scroll offsets from the scene's top-left corner.
function sceneView() {
  const viewport = $("viewport");
  return {
    zoom,
    scrollLeft: viewport.scrollLeft - pad.x,
    scrollTop: viewport.scrollTop - pad.y,
  };
}
function showScene(view) {
  setZoom(view.zoom);
  $("viewport").scrollLeft = view.scrollLeft + pad.x;
  $("viewport").scrollTop = view.scrollTop + pad.y;
}
function zoomAround(next, point) {
  const viewport = $("viewport");
  showScene(
    zoomAt(
      sceneView(),
      point ?? { x: viewport.clientWidth / 2, y: viewport.clientHeight / 2 },
      next,
    ),
  );
}
// Shows the whole graph, centred.
function fit() {
  const viewport = $("viewport");
  if (!viewport.clientWidth) return;
  showScene(
    frame(
      { width: viewport.clientWidth, height: viewport.clientHeight },
      { x: 0, y: 0, width: graphWidth, height: graphHeight },
      30,
    ),
  );
}
function zoomStep(direction) {
  zoomAround(zoom * (direction > 0 ? 1.25 : 0.8));
}
// The node an item shows: itself, or a wire's input node.
function nodeOfItem(item) {
  return (
    wireItems.get(item)?.node ?? visualNodes.find((node) => itemOf(node) === item)
  );
}
// Shift+2 frames the selected node; with nothing selected it fits all.
function zoomToSelection() {
  const node = nodeOfItem(focus.selected);
  if (!node) return fit();
  const viewport = $("viewport");
  showScene(
    frame(
      { width: viewport.clientWidth, height: viewport.clientHeight },
      { x: node.x, y: node.y, width: cardWidth, height: nodeHeight(node) },
    ),
  );
}
// The code pane: the workflow as authored source, lash's TypeScript or
// kernel text, linked to canonical nodes through printer line ranges.
function codeText() {
  if (!workflow) return undefined;
  if (codeView === "source") return workflow.authored?.text;
  if (codeView === "printed") return workflow.source;
  return workflow.text;
}
function codeNote() {
  if (codeView === "source") {
    const authored = workflow.authored;
    if (!authored) return "No authored source: this revision was opened as a kernel document.";
    const edited =
      authored.version === workflow.version
        ? ""
        : ` · edited since (revision ${workflow.version}); shown as imported`;
    return `Authored ${dialectName(authored.dialect)} imported at revision ${authored.version}${edited}${codeLinking ? " · linked through stored source map" : " · map unavailable for this revision"}`;
  }
  if (codeView === "printed")
    return workflow.source
      ? `Lash's exact ${dialectName(workflow.sourceDialect)} printer${codeLinking ? " · linked through its source map" : ""}`
      : `No printed source: ${workflow.sourceUnavailable}`;
  return `Kernel document text${codeLinking ? " · linked to coarse nodes" : " · not linked: no matching printer ranges"}`;
}
function renderCode() {
  if (!workflow) return;
  document
    .querySelectorAll("[data-code]")
    .forEach((button) =>
      button.setAttribute("aria-pressed", String(button.dataset.code === codeView)),
    );
  const text = codeText();
  codeLinking =
    codeView === "printed" && workflow.source
      ? readableLinking(viewOf(workflow))
      : codeView === "kernel"
        ? kernelLinking(workflow.text, viewOf(workflow))
        : sourceLinking(workflow);
  $("code-note").textContent = codeNote();
  $("code-note").title = codeNote();
  const language =
    codeView === "kernel"
      ? "kernel"
      : codeView === "source"
        ? (workflow.authored?.dialect ?? "typescript")
        : workflow.sourceDialect;
  $("code-view").innerHTML = text
    ? text
        .replace(/\n$/, "")
        .split("\n")
        .map(
          (line, index) =>
            `<div class="code-line${codeLinking?.siteAt(index) !== undefined ? " linked" : ""}" data-line="${index}"><span class="ln">${index + 1}</span><span class="code">${highlightLine(line, language)}</span></div>`,
        )
        .join("")
    : `<p class="code-empty">${codeNote()}</p>`;
  markCode();
}
// Marks the line the selected statement starts on; a compound statement
// spans its whole body, so only its first line is marked. With `reveal`
// naming the code pane, scrolls the line into view if it is not.
function markCode(reveal) {
  const view = $("code-view");
  view
    .querySelectorAll(".code-line.selected")
    .forEach((line) => line.classList.remove("selected", "open"));
  const span = selected && codeLinking?.lines(selected.site);
  const first = span && view.children[span.start];
  if (!first) return;
  first.classList.add("selected");
  first.classList.toggle("open", focus.open);
  if (reveal !== "code" && reveal !== "both") return;
  const top = first.offsetTop - view.offsetTop;
  if (top < view.scrollTop || top + first.offsetHeight > view.scrollTop + view.clientHeight)
    view.scrollTop = Math.max(0, top - view.clientHeight / 3);
}
// The statement an item is: a node's own, or a wire's input node's.
function statementOfItem(item) {
  if (item === undefined) return undefined;
  const wire = wireItems.get(item);
  if (wire) return wire.node.s;
  const carrier = viewOf(workflow).nodes.find((node) => key(node.site) === item || node.members.some((member) => key(member) === item));
  if (carrier) return statementAt(carrier.site);
  return undefined;
}
// The one place selection and focus change. `reveal` names the pane to
// scroll to the counterpart: "code", "canvas" or "both"; the pane the user
// acted in stays where it is.
function setFocus(next, reveal) {
  focus = next;
  selected = statementOfItem(focus.selected);
  if (!focus.open) {
    $("node-panel").hidden = true;
    $("node-form").replaceChildren();
  }
  renderSelection(reveal);
  applySelection();
  if (reveal === "canvas" || reveal === "both") revealItem(focus.selected);
  controls();
}
function selectItem(item, reveal) {
  if (item && !visualNodes.some((node) => itemOf(node) === item)) {
    const carrier = viewOf(workflow).nodes.find((node) => node.members.some((site) => key(site) === item));
    if (carrier) item = key(carrier.site);
  }
  setFocus(select(focus, item), reveal);
}
// Opens an item's inspector: a node's settings, or a connection's input.
function openItem(item) {
  if (busy || item === undefined) return;
  const wire = wireItems.get(item);
  const node = nodeOfItem(item);
  if (!node) {
    const statement = statementOfItem(item);
    if (statement)
      report(`${summary(statement.statement)} is not drawn as its own node in this scope.`);
    return;
  }
  connecting = undefined;
  setFocus(open(focus, item));
  if (wire) openPort(wire.node, wire.port);
  else openNode(node);
  applySelection();
}
function openPortOf(node, port) {
  if (busy) return;
  setFocus(open(focus, itemOf(node)));
  openPort(node, port);
  applySelection();
}
// Marks the selected item, and whether it is open, on the canvas and in
// the trace.
function applySelection() {
  for (const element of document.querySelectorAll("#scene [data-item], #trace [data-item]")) {
    const on = element.dataset.item === focus.selected;
    element.classList.toggle("selected", on);
    element.classList.toggle("open", on && focus.open);
  }
}
// Scrolls the canvas to an item's node when it is out of view.
function revealItem(item) {
  const node = nodeOfItem(item);
  if (!node) return;
  const viewport = $("viewport");
  const view = sceneView();
  const left = node.x * zoom - view.scrollLeft,
    top = node.y * zoom - view.scrollTop;
  if (
    left >= 0 &&
    top >= 0 &&
    left + cardWidth * zoom <= viewport.clientWidth &&
    top + 74 * zoom <= viewport.clientHeight
  )
    return;
  showScene({
    zoom,
    scrollLeft: node.x * zoom - viewport.clientWidth / 5,
    scrollTop: node.y * zoom - viewport.clientHeight / 3,
  });
}
// Shows a statement's scope on the canvas and its node in view.
function revealStatement(statement) {
  if (key(statement.block) !== key(activeScope)) {
    const scope = canvasScopes().find((scope) => key(scope.block) === key(statement.block));
    if (!scope) return;
    activeScope = scope.block;
    renderScopes();
    renderCanvas();
  }
  revealItem(key(statement.site));
}
// Selects the statement a code line or trace row shows, switching the
// canvas to its scope.
function selectStatementKey(siteKey, reveal) {
  const statement = statementOfItem(siteKey);
  if (!statement || busy) return;
  if (reveal === "canvas" || reveal === "both") revealStatement(statement);
  selectItem(key(statement.site), reveal);
  if (!visualNodes.some((node) => itemOf(node) === key(statement.site)))
    report(`${summary(statement.statement)} is not drawn as its own node in this scope.`);
}
const LAYOUT_KEY = "workflow-editor.layout",
  SPLIT_KEY = "workflow-editor.split";
function remembered(name) {
  try {
    return localStorage.getItem(name);
  } catch {
    return null;
  }
}
function remember(name, value) {
  try {
    localStorage.setItem(name, value);
  } catch {
    // Storage is a convenience: the layout still applies for this page.
  }
}
function setLayout(layout) {
  const previous = $("workspace").dataset.layout;
  $("workspace").dataset.layout = layout;
  document
    .querySelectorAll("[data-layout]")
    .forEach(
      (button) =>
        button.tagName === "BUTTON" &&
        button.setAttribute("aria-pressed", String(button.dataset.layout === layout)),
    );
  remember(LAYOUT_KEY, layout);
  if (previous === "code" && layout !== "code") fit();
}
function setSplit(fraction) {
  const split = Math.max(0.2, Math.min(0.8, fraction));
  $("workspace").style.setProperty("--split", `${(split * 100).toFixed(1)}%`);
  remember(SPLIT_KEY, String(split));
}
function setupCodePane() {
  for (const button of document.querySelectorAll("button[data-layout]"))
    button.onclick = () => setLayout(button.dataset.layout);
  const layout = remembered(LAYOUT_KEY);
  setLayout(["graph", "split", "code"].includes(layout) ? layout : "split");
  const split = Number(remembered(SPLIT_KEY));
  if (split) setSplit(split);
  for (const button of document.querySelectorAll("[data-code]"))
    button.onclick = () => {
      codeView = button.dataset.code;
      renderCode();
    };
  // A click on a linked line selects its statement; a double-click opens
  // its node.
  const lineSite = (e) => {
    const line = e.target.closest(".code-line");
    return line && codeLinking?.siteAt(Number(line.dataset.line));
  };
  $("code-view").onclick = (e) => {
    const siteKey = lineSite(e);
    if (siteKey) selectStatementKey(siteKey, "canvas");
  };
  $("code-view").ondblclick = (e) => {
    const siteKey = lineSite(e);
    if (siteKey) openItem(siteKey);
  };
  const divider = $("divider");
  divider.onpointerdown = (e) => {
    divider.setPointerCapture(e.pointerId);
    divider.classList.add("dragging");
  };
  divider.onpointermove = (e) => {
    if (!divider.hasPointerCapture(e.pointerId)) return;
    const bounds = $("workspace").getBoundingClientRect();
    setSplit((e.clientX - bounds.left) / bounds.width);
  };
  divider.onpointerup = divider.onpointercancel = () =>
    divider.classList.remove("dragging");
  divider.onkeydown = (e) => {
    const step = { ArrowLeft: -0.02, ArrowRight: 0.02 }[e.key];
    if (!step) return;
    e.preventDefault();
    const current = parseFloat(getComputedStyle($("workspace")).getPropertyValue("--split")) / 100;
    setSplit((current || 0.58) + step);
  };
}
// The source editor follows the host guide's authority rule: once a version
// has a change its imported module lacks (a canvas edit or a caption), it
// opens on lash's print of the document with its captions, so reading the
// source back keeps them.
function sourceNote(printed) {
  const changed = workflow.authored && workflow.authored.version !== workflow.version;
  const dropped = workflow.captionsDropped?.length
    ? ` ${workflow.captionsDropped.length} caption(s) cannot be written in this language and would be lost.`
    : "";
  if (printed)
    return `Lash's print of this version, with its captions.${changed ? " It has changes its imported module does not have." : ""}${dropped}`;
  return changed
    ? "The module as imported, before this version's canvas and caption changes: importing it replaces them."
    : "The module as imported. Captions are part of the source.";
}
function refreshSourceEditor() {
  const printed = $("source-kind").value === "printed";
  $("source-note").textContent = sourceNote(printed);
  $("caption-guide").textContent = workflow.captionGuide ?? "";
  $("source").value = printed ? (workflow.source ?? "") : (workflow.authored?.text ?? "");
  $("source-dialect").disabled = printed;
  if (printed) $("source-dialect").value = workflow.sourceDialect;
  $("import").textContent = printed ? "Read printed document" : "Import module";
  $("source").setAttribute("aria-label", `${dialectName($("source-dialect").value)} source`);
}
function setupVisual() {
  $("source-kind").onchange = refreshSourceEditor;
  $("source-dialect").onchange = () => $("source").setAttribute("aria-label", `${dialectName($("source-dialect").value)} source`);
  $("scope").onchange = () => {
    activeScope = canvasScopes()[Number($("scope").value)].block;
    setFocus(close(focus));
    renderCanvas();
    fit();
  };
  window.addEventListener("resize", fit);
  $("close-panel").onclick = () => setFocus(close(focus));
  $("source-button").onclick = () => $("source-editor").showModal();
  $("history-button").onclick = () => showTrace($("trace").hidden);
  $("trace-toggle").onclick = () => {
    const collapsed = document
      .querySelector(".canvas-area")
      .classList.toggle("trace-collapsed");
    $("trace-toggle").setAttribute("aria-expanded", String(!collapsed));
  };
  $("clear-run").onclick = () => {
    resetRun();
    renderCanvas();
    showTrace(false);
  };
  $("replay").oninput = () => scrub(Number($("replay").value));
  $("replay-back").onclick = () => scrub(Number($("replay").value) - 1);
  $("replay-forward").onclick = () => scrub(Number($("replay").value) + 1);
  document
    .querySelectorAll("[data-close]")
    .forEach(
      (button) => (button.onclick = () => $(button.dataset.close).close()),
    );
  $("example").onchange = () => loadExample($("example").value);
  $("dialect-typescript").onclick = () => chooseDialect("typescript");
  $("dialect-python").onclick = () => chooseDialect("python");
  $("add-step").onclick = () => {
    openCatalog(
      activeScope ? appendPosition(workflow, activeScope) : undefined,
    );
  };
  $("zoom-in").onclick = () => zoomStep(1);
  $("zoom-out").onclick = () => zoomStep(-1);
  $("fit").onclick = fit;
  const viewport = $("viewport");
  viewport.addEventListener(
    "wheel",
    (e) => {
      if (wheelIntent(e) !== "zoom") {
        // A two-finger scroll pans natively, and stops following the run.
        following = false;
        return;
      }
      e.preventDefault();
      const bounds = viewport.getBoundingClientRect();
      zoomAround(zoom * wheelFactor(e), {
        x: e.clientX - bounds.left,
        y: e.clientY - bounds.top,
      });
    },
    { passive: false },
  );
  // Canvas gestures, for mouse, pen and touch alike. A left press on
  // anything becomes a pan once it moves past the click slop; Space or the
  // middle button pans at once; two touches pinch and pan. A pan never
  // selects. Each press is preventDefault-ed, so no drag selects text.
  const pointers = new Map();
  let gesture,
    pinch,
    suppressClick = false,
    hand = false,
    over = false;
  const at = (e) => ({ x: e.clientX, y: e.clientY });
  const setHand = (on) => {
    hand = on;
    viewport.classList.toggle("hand", on);
  };
  const beginPan = (e) => {
    gesture.panning = true;
    following = false;
    viewport.setPointerCapture(e.pointerId);
    viewport.classList.add("panning");
  };
  viewport.addEventListener("pointerenter", () => (over = true));
  viewport.addEventListener("pointerleave", () => (over = false));
  viewport.addEventListener("pointerdown", (e) => {
    if (e.target.closest("input, select, textarea")) return;
    e.preventDefault();
    // A press on the canvas also drops text selected elsewhere on the page.
    window.getSelection()?.removeAllRanges();
    viewport.focus({ preventScroll: true });
    suppressClick = false;
    pointers.set(e.pointerId, at(e));
    if (pointers.size === 2) {
      // A second finger turns the gesture into a pinch.
      gesture = undefined;
      pinch = [...pointers.values()];
      for (const id of pointers.keys()) viewport.setPointerCapture(id);
      following = false;
      return;
    }
    if (pointers.size > 2 || (e.button !== 0 && e.button !== 1)) return;
    gesture = {
      id: e.pointerId,
      start: at(e),
      left: viewport.scrollLeft,
      top: viewport.scrollTop,
      panning: false,
    };
    if (hand || e.button === 1) beginPan(e);
  });
  viewport.addEventListener("pointermove", (e) => {
    if (!pointers.has(e.pointerId)) return;
    pointers.set(e.pointerId, at(e));
    if (pinch && pointers.size === 2) {
      const next = [...pointers.values()];
      const step = pinchStep(pinch, next);
      pinch = next;
      viewport.scrollLeft -= step.pan.x;
      viewport.scrollTop -= step.pan.y;
      const bounds = viewport.getBoundingClientRect();
      zoomAround(zoom * step.factor, {
        x: step.center.x - bounds.left,
        y: step.center.y - bounds.top,
      });
      return;
    }
    if (!gesture || gesture.id !== e.pointerId) return;
    if (!gesture.panning && dragged(gesture.start, at(e))) beginPan(e);
    if (gesture.panning) {
      viewport.scrollLeft = gesture.left - (e.clientX - gesture.start.x);
      viewport.scrollTop = gesture.top - (e.clientY - gesture.start.y);
    }
  });
  const release = (e) => {
    if (!pointers.delete(e.pointerId)) return;
    if (pinch) {
      // The click a lifted pinch would make is not a click.
      suppressClick = true;
      if (pointers.size < 2) pinch = undefined;
      return;
    }
    if (gesture?.panning) suppressClick = true;
    gesture = undefined;
    viewport.classList.remove("panning");
  };
  viewport.addEventListener("pointerup", release);
  viewport.addEventListener("pointercancel", release);
  // The click ending a pan lands on the viewport: drop it before any item
  // sees it.
  viewport.addEventListener(
    "click",
    (e) => {
      if (!suppressClick) return;
      suppressClick = false;
      e.stopImmediatePropagation();
      e.preventDefault();
    },
    true,
  );
  viewport.addEventListener("click", (e) => {
    if (e.target.closest("[data-item], button")) return;
    // A click on empty canvas clears the selection and any pending
    // connection.
    connecting = undefined;
    setFocus(NOTHING);
  });
  window.addEventListener("blur", () => setHand(false));
  document.addEventListener("keyup", (e) => {
    if (e.code === "Space") setHand(false);
  });
  document.addEventListener("keydown", (e) => {
    if (document.querySelector("dialog[open]")) return;
    if (e.key === "Escape") {
      if (connecting) {
        connecting = undefined;
        report("");
        document
          .querySelectorAll(".data-port.chosen")
          .forEach((port) => port.classList.remove("chosen"));
      } else setFocus(escape(focus));
      return;
    }
    if (e.target.closest?.("input, textarea, select, [contenteditable]")) return;
    const canvas = $("workspace").dataset.layout !== "code";
    if ((e.ctrlKey || e.metaKey) && canvas && ["=", "+", "-", "0"].includes(e.key)) {
      e.preventDefault();
      if (e.key === "0") zoomAround(1);
      else zoomStep(e.key === "-" ? -1 : 1);
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (e.shiftKey && canvas && (e.code === "Digit1" || e.code === "Digit2")) {
      e.preventDefault();
      if (e.code === "Digit1") fit();
      else zoomToSelection();
      return;
    }
    // Enter and Space keep activating a focused control.
    if (e.target.closest?.("button, a, summary")) return;
    if (e.code === "Space" && canvas && (over || viewport.contains(e.target))) {
      e.preventDefault();
      setHand(true);
      return;
    }
    if (e.key === "Enter" && focus.selected !== undefined && !focus.open) {
      e.preventDefault();
      openItem(focus.selected);
      return;
    }
    if (!e.key.startsWith("Arrow")) return;
    if ($("code-view").contains(e.target)) {
      if (!codeLinking || !["ArrowUp", "ArrowDown"].includes(e.key)) return;
      const next = codeLinking.step(
        selected && key(selected.site),
        e.key === "ArrowDown" ? 1 : -1,
      );
      if (!next) return;
      e.preventDefault();
      selectStatementKey(next, "both");
    } else if (canvas) {
      e.preventDefault();
      const centres = visualNodes.map((node) => ({
        x: node.x + cardWidth / 2,
        y: node.y + nodeHeight(node) / 2,
        node,
      }));
      const current = nodeOfItem(focus.selected);
      const from = centres.find((centre) => centre.node === current);
      const next = from ? neighbour(centres, from, e.key) : centres[0];
      if (next) setFocus(select(focus, itemOf(next.node)), "both");
    }
  });
}

$("run").onclick = () => run();
setupVisual();
setupCodePane();
controls();
await action(async () => {
  show(await call("/workflow"));
  examples = await call("/workflows");
  renderExamples();
  renderDialect();
  report("");
});

fit();
