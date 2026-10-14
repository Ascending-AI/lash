import {
  buildDataflow,
  availableName,
  planInputEdit,
  planEndAfter,
  encodeValue as expression,
  sourceOptions,
  typeName,
  expressionName,
  constantValue,
  fieldsOf,
} from "./authoring-dataflow.mjs";
import {
  actionOf,
  recordInput as canonicalRecordInput,
  literal,
  startPosition as scopeStart,
  appendPosition,
} from "./authoring-projection.mjs";
const $ = (id) => document.getElementById(id);
const key = (site) => JSON.stringify([site.unit, site.path ?? []]);
let workflow,
  templates = [],
  calls = new Map(),
  runInputs = {},
  dataView,
  connecting,
  finalOutput,
  observationGap = false,
  expandedValues = new Map(),
  selected,
  steps = [],
  plan,
  tab = "source",
  busy = false,
  running = false;
let statuses = new Map(),
  statementNodes = new Map();
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
      "[data-write], #queue button, #node-panel .save-node, .add-port",
    )
    .forEach((button) => (button.disabled = busy || running || !workflow));
  $("apply").disabled = busy || running || !plan;
  $("preview").disabled = busy || running || !steps.length;
  $("run").disabled = busy || running || !workflow?.definition;
  $("rename").disabled ||= !statementNodes.get(
    key(selected?.site ?? { unit: "main" }),
  )?.let;
  $("replace").disabled ||= !selected;
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
  const retainedExpanded = new Map();
  for (const site of expandedValues.values()) {
    const mapped = correspondence?.entries?.find(
      (entry) => key(entry.from) === key(site),
    )?.to;
    if (mapped) retainedExpanded.set(key(mapped), mapped);
  }
  expandedValues = retainedExpanded;
  workflow = next;
  if (!sameEntry || !correspondence) runInputs = initialInputs(next);
  connecting = undefined;
  finalOutput = undefined;
  observationGap = false;
  statuses = new Map();
  calls = new Map();
  statementNodes = new Map(
    next.statements.map((item) => [key(item.site), item.statement]),
  );
  const mapped =
    old &&
    correspondence?.entries?.find((item) => key(item.from) === key(old))?.to;
  selected =
    next.statements.find(
      (stmt) =>
        key(stmt.site) ===
        key((correspondence ? mapped : old) ?? { unit: "main" }),
    ) ?? next.statements[0];
  $("revision").textContent = "All changes saved";
  $("admission").textContent = next.definition
    ? "Ready to run"
    : `Not admitted: ${next.notAdmitted}`;
  $("source").value = next.source ?? "";
  $("source").scrollTop = 0;
  $("source").scrollLeft = 0;
  $("entry").value = next.entry;
  $("direct").value = "[]";
  $("run-status").textContent = "Not started";
  $("run-output").textContent = "Run this revision to see its results.";
  const mappedScope =
    previousScope &&
    correspondence?.entries?.find(
      (item) => key(item.from) === key(previousScope),
    )?.to;
  activeScope =
    next.scopes.find(
      (scope) => mappedScope && key(scope.block) === key(mappedScope),
    )?.block ?? next.scopes[0]?.block;
  renderScopes();
  renderCanvas();
  renderPlacement();
  renderSelection();
  renderPane();
  controls();
}
function renderPlacement() {
  $("placement").replaceChildren(
    ...workflow.statements.map((statement, index) => {
      const option = document.createElement("option");
      option.value = String(index);
      option.textContent = `${"↳ ".repeat(statement.depth)}${statement.summary}`;
      option.selected = selected && key(statement.site) === key(selected.site);
      return option;
    }),
  );
}
function renderSelection() {
  const stmt = selected && statementNodes.get(key(selected.site));
  $("selected-kind").textContent = stmt ? Object.keys(stmt)[0] : "—";
  $("selection").textContent = selected
    ? `${selected.summary}\nSite ${JSON.stringify(selected.site)}${stmt?.let ? `\nBinding: ${stmt.let.name}` : ""}`
    : "Select a statement on the canvas.";
  $("selection").style.whiteSpace = "pre-wrap";
}
function renderPane() {
  document
    .querySelectorAll("[data-tab]")
    .forEach((button) =>
      button.classList.toggle("active", button.dataset.tab === tab),
    );
  $("source").hidden = tab !== "source";
  $("source-actions").hidden = tab !== "source";
  $("code").hidden = tab === "source";
  $("code").textContent =
    tab === "document"
      ? JSON.stringify(workflow?.document, null, 2)
      : tab === "transaction"
        ? plan
          ? JSON.stringify(plan.transaction, null, 2)
          : "Preview pending changes to inspect the typed transaction."
        : (plan?.preview?.source ??
          "Preview pending changes to inspect the resulting TypeScript.");
}
function invalidate() {
  plan = undefined;
  renderQueue();
  renderPane();
  controls();
}
function renderQueue() {
  $("queue-count").textContent =
    `${steps.length} operation${steps.length === 1 ? "" : "s"}`;
  $("queue").replaceChildren(
    ...steps.map((step, index) => {
      const row = document.createElement("div");
      row.className = "queue-step";
      const label = document.createElement("span");
      label.textContent = `${index + 1}. ${step.effect ? step.effect.effect : Object.keys(step.direct)[0]}`;
      const remove = document.createElement("button");
      remove.textContent = "×";
      remove.setAttribute("aria-label", `Remove operation ${index + 1}`);
      remove.disabled = busy || running;
      remove.onclick = () => {
        steps.splice(index, 1);
        invalidate();
      };
      row.append(label, remove);
      return row;
    }),
  );
  if (!steps.length)
    $("queue").textContent = "Queue helpers and direct edits in any order.";
}
$("queue-helper").onclick = () =>
  action(async () => {
    const target = workflow.statements[Number($("placement").value)];
    if (!target) throw Error("Select an insertion position.");
    const input = JSON.parse($("payload").value);
    const effect = {
      at: { block: target.block, before: target.site },
      effect: $("effect").value,
      args: [$("input-mode").value === "json" ? expression(input) : input],
      result: "any",
    };
    effect.binding = $("binding").value.trim();
    if (!effect.binding)
      throw Error("Enter a result binding before queuing this effect.");
    steps.push({ effect });
    invalidate();
    report("Effect queued. Preview checks the complete transaction.");
  });
$("queue-direct").onclick = () =>
  action(async () => {
    const edits = JSON.parse($("direct").value);
    if (!Array.isArray(edits) || !edits.length)
      throw Error("Enter a non-empty JSON list of typed edits.");
    steps.push(...edits.map((direct) => ({ direct })));
    invalidate();
    report("Direct edits queued in order.");
  });
$("rename").onclick = () => {
  const stmt = statementNodes.get(key(selected.site));
  if (!stmt?.let) return;
  $("direct").value = JSON.stringify(
    [
      {
        rename_variable: {
          declared_at: selected.site,
          name: stmt.let.name,
          to: `${stmt.let.name}_renamed`,
        },
      },
    ],
    null,
    2,
  );
};
$("replace").onclick = () => {
  const stmt = statementNodes.get(key(selected.site));
  $("direct").value = JSON.stringify(
    [{ replace_statement: { statement: selected.site, with: stmt } }],
    null,
    2,
  );
};
$("effect").onchange = () => {
  $("input-mode").value = "json";
  $("payload").value = JSON.stringify(presets[$("effect").value], null, 2);
};
$("input-mode").onchange = () => {
  $("input-help").textContent =
    $("input-mode").value === "json"
      ? "Records and lists are expanded by the authoring helper."
      : 'One canonical Expr, e.g. {"variable":"customer"}. Names resolve in the target scope.';
};
$("placement").onchange = () => {
  selected = workflow.statements[Number($("placement").value)];
  renderCanvas();
  renderSelection();
  controls();
};
$("preview").onclick = () =>
  action(async () => {
    plan = undefined;
    plan = await call("/prototype/plan", { version: workflow.version, steps });
    renderPane();
    report(
      `Preview ready · ${plan.transaction.edits.length} typed edits · saved revision ${workflow.version} unchanged.`,
    );
  });
$("apply").onclick = () =>
  action(async () => {
    if (!plan) return;
    const answer = await call("/workflow/edits", {
      version: plan.version,
      edits: plan.transaction.edits,
    });
    steps = [];
    plan = undefined;
    show(answer.workflow, answer.correspondence);
    renderQueue();
    report(
      `Applied revision ${workflow.version}. Generated statements can now be selected and edited directly.`,
    );
  });
$("clear").onclick = () => {
  steps = [];
  invalidate();
  report("Pending changes cleared.");
};
$("import").onclick = () =>
  action(async () => {
    if (steps.length)
      throw Error("Apply or clear pending changes before importing source.");
    const next = await call("/prototype/source", {
      version: workflow.version,
      source: $("source").value,
      entry: $("entry").value,
    });
    plan = undefined;
    selected = undefined;
    show(next);
    report(`Imported TypeScript · revision ${workflow.version}.`);
  });
$("load").onclick = () =>
  action(async () => {
    if (steps.length)
      throw Error("Apply or clear pending changes before loading an example.");
    const template = templates.find((t) => t.id === $("template").value);
    if (!template) throw Error("Choose an example.");
    const next = template.canonical
      ? await call("/workflow/select", { id: template.id })
      : await call("/prototype/source", {
          version: workflow.version,
          source: template.source,
          entry: template.entry,
        });
    selected = undefined;
    show(next);
    plan = undefined;
    renderPane();
    report(`Loaded ${template.name}.`);
  });
for (const button of document.querySelectorAll("[data-tab]"))
  button.onclick = () => {
    tab = button.dataset.tab;
    renderPane();
  };
function output(text, error = false) {
  const line = document.createElement("div");
  line.className = `run-line${error ? " error" : ""}`;
  line.textContent = text;
  $("run-output").append(line);
}
async function run() {
  running = true;
  controls();
  statuses = new Map();
  calls = new Map();
  finalOutput = undefined;
  observationGap = false;
  renderInvocationOutput();
  if (selected) renderCallData(selected);
  renderCanvas();
  $("run-output").replaceChildren();
  $("run-status").textContent = "Running";
  const version = workflow.version;
  let sawTerminal = false;
  let observing = false;
  const traceLinks = new Map();
  function observe(event) {
    if (event.workflowVersion !== version) {
      output("Ignored an event for a different workflow revision.", true);
      return;
    }
    if (event.reset) {
      statuses.clear();
      calls.clear();
      traceLinks.clear();
      finalOutput = undefined;
      observationGap = true;
      sawTerminal = false;
      $("approve").hidden = true;
      $("approve").onclick = null;
      $("run-output").replaceChildren();
      $("run-status").textContent = "Recovering observation";
      output(
        event.error ??
          "Observation gap: live evidence was discarded; retained outcomes follow.",
      );
      const display = event.display ?? {};
      for (const message of display.messages ?? []) output(message);
      for (const [name, value] of Object.entries(display.statuses ?? {}))
        output(`${name}: ${value}`);
      for (const [name, items] of Object.entries(display.lists ?? {}))
        output(`${name}: ${items.join(", ")}`);
      for (const [name, value] of Object.entries(display.lights ?? {}))
        output(`${name}: ${value}`);
      if (display.progress !== undefined)
        output(`Progress ${display.progress}%`);
      renderInvocationOutput();
      if (selected) renderCallData(selected);
    }
    if (event.output !== undefined) {
      finalOutput = event.output;
      renderInvocationOutput();
    }
    if (event.site && !event.callOnly)
      statuses.set(key(event.site), event.status);
    if (event.site && event.callId) {
      const site = key(event.site);
      const occurrences = calls.get(site) ?? new Map();
      occurrences.set(event.callId, event);
      calls.set(site, occurrences);
      const node = visualNodes.find(
        (node) => node.s?.action && key(node.s.action) === site,
      );
      if (node) {
        let button = traceLinks.get(event.callId);
        if (!button) {
          button = el("button", "trace-link");
          button.onclick = () => {
            $("execution").close();
            openNode(node);
          };
          traceLinks.set(event.callId, button);
          $("run-output").append(button);
        }
        button.textContent = `${node.info.name} · ${event.status} · View data`;
      }
      if (selected?.action && key(selected.action) === site)
        renderCallData(selected);
    }
    if (!event.site && !event.reset && !event.callOnly) {
      $("run-status").textContent = event.status;
      if (event.status === "succeeded" || event.status === "failed")
        sawTerminal = true;
      output(
        `${event.status}${event.error ? `: ${event.error}` : ""}`,
        event.status === "failed",
      );
    }
    const delta = event.displayDelta ?? {};
    for (const message of delta.messagesAppended ?? []) output(message);
    for (const [name, value] of Object.entries(delta.statuses ?? {}))
      output(`${name}: ${value}`);
    for (const [name, items] of Object.entries(delta.listItemsAppended ?? {}))
      output(`${name}: ${items.join(", ")}`);
    for (const [name, value] of Object.entries(delta.lights ?? {}))
      output(`${name}: ${value}`);
    if (delta.progress !== undefined) output(`Progress ${delta.progress}%`);
    if (event.approvalKey) {
      $("approve").hidden = false;
      $("approve").onclick = () =>
        action(async () => {
          await call(`/approvals/${encodeURIComponent(event.approvalKey)}`, {
            approved: true,
          });
          $("approve").hidden = true;
        });
    }
    renderCanvas();
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
      output(
        "Observation ended before a terminal event. The process may still be running.",
        true,
      );
    }
  } catch (error) {
    if (!sawTerminal)
      $("run-status").textContent = observing
        ? "Observation unavailable"
        : "Could not start";
    output(errorText(error), true);
  } finally {
    running = false;
    renderInvocationOutput();
    if (selected) renderCallData(selected);
    $("approve").hidden = true;
    controls();
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
function renderScopes() {
  $("scope").replaceChildren(
    ...workflow.scopes.map((scope, index) => {
      const option = el(
        "option",
        null,
        scope.label === "Entry" ? "Workflow entry" : `${scope.label} ${index}`,
      );
      option.value = String(index);
      option.selected = key(scope.block) === key(activeScope);
      return option;
    }),
  );
}
function model(s) {
  const perform = actionOf(s)?.perform;
  if (perform) {
    const info = catalog[perform.effect] ?? {
      name: perform.effect,
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
  if (s.statement.if)
    return {
      s,
      info: { name: "Condition", app: "Flow", icon: "branch" },
      title: "If · Choose a path",
      subtitle: "True or false",
    };
  const action = actionOf(s);
  const kind =
    typeof action === "string" ? action : Object.keys(action ?? s.statement)[0];
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
  const name = names[kind] ?? kind;
  return {
    s,
    info: { name, app: "Flow", icon: "status" },
    title: name,
    subtitle: "Select to inspect",
    generic: true,
  };
}
function project() {
  dataView = buildDataflow(
    workflow,
    activeScope,
    new Set(expandedValues.keys()),
  );
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
  const index = workflow.statements.indexOf(s);
  const next = workflow.statements
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
    (node.outputs.length ? 21 : 0)
  );
}
function drawNode(node, x, y) {
  const card = el("section", "data-node " + node.kind);
  card.style.left = x + "px";
  card.style.top = y + "px";
  const state = node.s?.action && statuses.get(key(node.s.action));
  if (state) {
    card.classList.add(state);
    if (state === "incomplete")
      card.title = "Incomplete evidence: this step's outcome was not observed.";
  }
  if (
    node.s &&
    selected &&
    key(node.s.site) === key(selected.site) &&
    !$("node-panel").hidden
  )
    card.classList.add("selected");
  const header = el("button", "data-node-header");
  header.setAttribute("aria-label", node.title);
  header.append(icon(node.info.icon));
  const heading = el("span");
  heading.append(
    el("strong", null, node.title),
    el("small", null, node.subtitle),
  );
  header.append(heading);
  if (state)
    header.append(
      el(
        "span",
        "data-status",
        state === "succeeded"
          ? "✓"
          : state === "failed"
            ? "!"
            : state === "incomplete"
              ? "?"
              : "…",
      ),
    );
  header.onclick = () => openNode(node);
  card.append(header);
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
      button.onclick = () => {
        if (direction === "input") {
          if (connecting) connectPort(port, connecting);
          else openPort(node, port);
        } else if (port.discarded || port.stored) openNode(node);
        else {
          connecting = port;
          $("node-panel").hidden = true;
          report(
            `Connect ${port.name}: choose an input port, or press Escape to cancel.`,
          );
          if (node.s) {
            const end = el("button", "end-output", "End here with this output");
            end.disabled = busy || running;
            end.onclick = () => openEndStep(node, port);
            $("result").append(end);
          }
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
    for (const prior of ends) controlWire(prior, placed, prior.label);
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
      const tag =
        typeof node.s?.statement === "string"
          ? node.s.statement
          : Object.keys(node.s?.statement ?? {})[0];
      if (
        node.kind === "output" ||
        ["throw", "fail", "finish", "break", "continue"].includes(tag)
      )
        return [];
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
        edge.setAttribute(
          "d",
          point.x > source.x
            ? `M${source.x},${source.y} C${source.x + 90},${source.y} ${point.x - 90},${point.y} ${point.x},${point.y}`
            : `M${source.x},${source.y} C${source.x + 60},${lift} ${point.x - 60},${lift} ${point.x},${point.y}`,
        );
        edge.setAttribute("class", "data-wire");
        edge.setAttribute("marker-end", "url(#data-arrow)");
        const title = document.createElementNS(ns, "title");
        title.textContent = `${sourceNode.title}.${source.port.name} → ${node.title}.${point.port.name}`;
        edge.append(title);
        $("wires").append(edge);
      }
    }
}
function renderCanvas() {
  if (!workflow) return;
  visualNodes = [];
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
    : workflow.dataflow.bindings.filter(
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
  $("node-count").textContent =
    `${visualNodes.length} nodes · Select an input to edit its source`;
  document.querySelector("h1").textContent = workflow.entry.replaceAll(
    "_",
    " ",
  );
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
  if (steps.length)
    throw Error("Apply or clear the pending changes in Advanced first.");
  const planned = await call("/prototype/plan", {
    version: workflow.version,
    steps: changes,
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
  renderCanvas();
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
      let i = 1;
      const names = new Set(
        workflow.statements.map((s) => s.statement.let?.name),
      );
      while (names.has("step_" + i)) i++;
      await applyVisual([
        {
          effect: {
            at: insertAt,
            effect,
            args: [expression(values)],
            result: "any",
            binding: "step_" + i,
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
  const records = [
    ...(calls.get(key(statement.action ?? statement.site))?.values() ?? []),
  ];
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
  const presets = {
    deep_research: { topic: "Reducing customer onboarding drop-off" },
    weekly_kpis: { workspace: "Acme Cloud" },
  };
  return Object.fromEntries(
    (next.document.entries[next.entry]?.params ?? []).map((param) => [
      param.name,
      presets[next.entry]?.[param.name] ?? defaultValue(param.ty),
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
    input.value = scalar
      ? String(value ?? defaultValue(type))
      : JSON.stringify(value ?? defaultValue(type), null, 2);
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
  const params = workflow.document.entries[workflow.entry]?.params ?? [];
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
function typeCompatible(expected, actual) {
  if (!expected || !actual || expected === "any" || actual === "any")
    return true;
  if (JSON.stringify(expected) === JSON.stringify(actual)) return true;
  if (expected === "number" && ["int", "float"].includes(actual)) return true;
  if (expected?.union)
    return expected.union.some((t) => typeCompatible(t, actual));
  if (actual?.union)
    return actual.union.every((t) => typeCompatible(expected, t));
  if (expected === "text" && actual?.enum) return true;
  if (expected?.enum && actual === "text") return true;
  if (expected?.enum && actual?.enum)
    return actual.enum.every((v) => expected.enum.includes(v));
  if (expected?.list && actual?.list)
    return typeCompatible(expected.list, actual.list);
  if (expected?.record && actual?.record) {
    return fieldsOf(expected).every((field) => {
      const match = fieldsOf(actual).find((f) => f.name === field.name);
      return match
        ? typeCompatible(field.ty, match.ty)
        : field.optional || !!actual.record.rest;
    });
  }
  return false;
}
function freshBinding(base = "value") {
  return availableName(workflow, base);
}
function replacePort(port, value) {
  return planInputEdit(workflow, port, value).map((direct) => ({ direct }));
}
function connectPort(port, source) {
  if (busy || running) return;
  action(async () => {
    if (!(port.available ?? []).includes(source.binding))
      throw Error(
        "This value is not available at the selected input. Choose a value in scope that is defined before this step.",
      );
    if (!typeCompatible(port.acceptsType ?? port.type, source.type))
      throw Error(
        `Cannot connect ${typeName(source.type)} to ${typeName(port.type)}.`,
      );
    await applyVisual(replacePort(port, source.expression));
    connecting = undefined;
    report(`Connected ${source.name} to ${port.name}`);
  });
}
function openPort(node, port) {
  if (busy) return;
  selected = node.s;
  const container = panel(
    node.kind === "output" ? "RESULT MAPPING" : "EDIT INPUT",
  );
  identity(container, node.info);
  container.append(el("h3", null, `${port.name} · ${typeName(port.type)}`));
  const modeLabel = el("label", null, "Value source");
  modeLabel.htmlFor = "mapping-mode";
  const mode = el("select");
  mode.id = "mapping-mode";
  for (const [value, label] of [
    ["constant", "Constant"],
    ["connection", "Connected value"],
    ["expression", "Expression (advanced)"],
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
      : "expression";
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
        read = () => expression(readValue(), currentType);
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
        item.disabled = !typeCompatible(
          port.acceptsType ?? port.type,
          option.type,
        );
        select.append(item);
      });
      const first = options.findIndex((o) =>
        typeCompatible(port.acceptsType ?? port.type, o.type),
      );
      select.value = String(current >= 0 ? current : first);
      editor.append(
        label,
        select,
        el(
          "p",
          "help",
          "Only values visible at this input are selectable. Any means the type is not known statically.",
        ),
      );
      read = () => {
        const option = options[Number(select.value)];
        if (!option)
          throw Error(
            "No source is available. Define a value before this step.",
          );
        if (!typeCompatible(port.acceptsType ?? port.type, option.type))
          throw Error("The selected types do not match.");
        return option.expression;
      };
    } else {
      const label = el("label", null, "Canonical expression");
      label.htmlFor = "mapping-expression";
      const input = el("textarea");
      input.id = "mapping-expression";
      input.value = JSON.stringify(port.expression, null, 2);
      editor.append(
        label,
        input,
        el(
          "p",
          "help",
          "This edits a canonical expression. Library calls remain opaque; the transaction is checked by Lash.",
        ),
      );
      read = () => JSON.parse(input.value);
    }
  }
  mode.onchange = draw;
  draw();
  const save = el("button", "primary save-node", "Save mapping");
  save.onclick = () => action(() => applyVisual(replacePort(port, read())));
  container.append(save);
  const details = el("details", "promote-value");
  details.append(el("summary", null, "Make this a named value"));
  const nameLabel = el("label", null, "Value name");
  nameLabel.htmlFor = "value-name";
  const name = el("input");
  name.id = "value-name";
  name.value = freshBinding(
    port.name.replace(/[^a-zA-Z0-9_]/g, "_") + "_value",
  );
  const promote = el("button", "save-node", "Create value node");
  promote.onclick = () =>
    action(async () => {
      const binding = name.value.trim();
      if (!/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(binding))
        throw Error("Use a name starting with a letter or underscore.");
      if (workflow.dataflow.bindings.some((b) => b.name === binding))
        throw Error("Choose a new value name.");
      const changes = [
        {
          direct: {
            insert_statement: {
              at: { block: port.context.block, before: port.context.statement },
              statement: { let: { name: binding, value: { expr: read() } } },
            },
          },
        },
        ...replacePort(port, { variable: binding }),
      ];
      await applyVisual(changes);
      const created = workflow.dataflow.bindings.find(
        (item) => item.name === binding,
      );
      if (created) expandedValues.set(key(created.site), created.site);
      renderCanvas();
      report(`Created ${binding}`);
    });
  details.append(nameLabel, name, promote);
  container.append(details);
  renderCanvas();
  controls();
}

function openEndStep(node, source) {
  if (busy || running) return;
  connecting = undefined;
  report("");
  document
    .querySelectorAll(".data-port.chosen")
    .forEach((port) => port.classList.remove("chosen"));
  selected = node.s;
  const container = panel("END HERE");
  identity(container, node.info);
  container.append(el("h3", null, `${source.name} · ${typeName(source.type)}`));
  const label = el("label", null, "End behavior");
  label.htmlFor = "end-operation";
  const select = el("select");
  select.id = "end-operation";
  for (const [value, text] of [
    ["finish", "Finish the whole invocation"],
    ["return", "Return from this function"],
  ]) {
    const option = el("option", null, text);
    option.value = value;
    select.append(option);
  }
  const explanation = el("p", "help");
  const update = () => {
    explanation.textContent =
      select.value === "finish"
        ? "Insert Finish immediately after this step. Later steps will not run. Inside a nested function, this also ends its caller's invocation. Finish skips finally blocks."
        : "Insert Return immediately after this step. Ordinary later steps in this function will not run. Enclosing finally blocks still run and can change the result; a nested function's caller can continue.";
  };
  select.onchange = update;
  update();
  const save = el("button", "primary save-node", "End here with this output");
  save.onclick = () =>
    action(() =>
      applyVisual(
        planEndAfter(workflow, node, source, select.value).map((direct) => ({
          direct,
        })),
      ),
    );
  container.append(
    label,
    select,
    explanation,
    el(
      "p",
      "help",
      "An immediately following Return or Finish is replaced. Other later statements stay in the document; deleting the terminal step makes them reachable again.",
    ),
    save,
  );
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
function openNode(node) {
  if (busy) return;
  selected = node.s;
  renderPlacement();
  renderSelection();
  const container = panel(
    node.kind === "input"
      ? "WORKFLOW INPUTS"
      : node.kind === "output"
        ? "TERMINAL STEP"
        : "NODE SETTINGS",
  );
  identity(container, node.info);
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
      button.onclick = () => openPort(node, port);
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
    const returnable = node.outputs.filter(
      (port) => port.binding !== undefined && !port.discarded && !port.stored,
    );
    if (node.s && returnable.length) {
      container.append(el("h3", null, "End with an output"));
      for (const port of returnable) {
        const end = el(
          "button",
          "mapping-button",
          `${port.name} · End here with this output`,
        );
        end.setAttribute("aria-label", `End here with ${port.name}`);
        end.onclick = () => openEndStep(node, port);
        container.append(end);
      }
    }
    if (node.kind === "output" && !node.workflowOutput)
      container.append(
        el(
          "p",
          "help",
          "This returns to the enclosing function’s caller. Its result is not the workflow invocation result.",
        ),
      );
    const foldedBindings = [...dataView.hidden]
      .map((id) => dataView.bindings.get(id))
      .filter((binding) =>
        node.inputs.some(
          (port) =>
            port.context && key(port.context.statement) === key(binding.site),
        ),
      );
    for (const binding of foldedBindings) {
      const expand = el("button", null, `Show ${binding.name} as a node`);
      expand.onclick = () => {
        expandedValues.set(key(binding.site), binding.site);
        $("node-panel").hidden = true;
        renderCanvas();
      };
      container.append(expand);
    }
    if (node.binding) {
      const label = el("label", null, "Output name");
      label.htmlFor = "output-name";
      const input = el("input");
      input.id = "output-name";
      input.value = node.binding.name;
      const rename = el("button", "save-node", "Rename value");
      rename.onclick = () =>
        action(() =>
          applyVisual([
            {
              direct: {
                rename_variable: {
                  declared_at: node.binding.site,
                  name: node.binding.name,
                  to: input.value.trim(),
                },
              },
            },
          ]),
        );
      container.append(label, input, rename);
      if (node.binding.assigned)
        container.append(
          el(
            "p",
            "help",
            "This variable is reassigned. Data wires identify the binding; its value depends on execution order.",
          ),
        );
    }
    if (node.s?.action) {
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
          "Nested blocks are available from the scope menu. Advanced exposes the complete canonical action.",
        ),
      );
      const advanced = el("button", null, "Open in Advanced");
      advanced.onclick = () => {
        $("advanced").showModal();
        $("replace").click();
        $("direct-details").open = true;
      };
      container.append(advanced);
    }
    if (node.s) {
      const remove = el("button", "danger save-node", "Delete node");
      remove.onclick = () =>
        action(() =>
          applyVisual([
            { direct: { remove_statement: { statement: node.s.site } } },
          ]),
        );
      container.append(remove);
    }
  }
  renderCanvas();
  controls();
}
function setZoom(value) {
  zoom = Math.max(0.3, Math.min(1.6, value));
  $("scene").style.transform = `scale(${zoom})`;
  $("zoom-label").textContent = Math.round(zoom * 100) + "%";
}
function fit() {
  const viewport = $("viewport");
  setZoom(
    Math.min(
      1,
      (viewport.clientWidth - 60) / graphWidth,
      (viewport.clientHeight - 80) / graphHeight,
    ),
  );
  viewport.scrollLeft = 0;
  viewport.scrollTop = 0;
}
function setupVisual() {
  $("scope").onchange = () => {
    activeScope = workflow.scopes[Number($("scope").value)].block;
    $("node-panel").hidden = true;
    renderCanvas();
    controls();
    fit();
  };
  window.addEventListener("resize", fit);
  $("close-panel").onclick = () => {
    $("node-panel").hidden = true;
    renderCanvas();
  };
  $("examples-button").onclick = () => $("examples").showModal();
  $("advanced-button").onclick = () => $("advanced").showModal();
  $("history-button").onclick = () => $("execution").showModal();
  document
    .querySelectorAll("[data-close]")
    .forEach(
      (button) => (button.onclick = () => $(button.dataset.close).close()),
    );
  const load = $("load").onclick;
  $("load").onclick = async () => {
    await load();
    $("examples").close();
    $("node-panel").hidden = true;
    fit();
  };
  $("add-step").onclick = () => {
    openCatalog(
      activeScope ? appendPosition(workflow, activeScope) : undefined,
    );
  };
  $("zoom-in").onclick = () => setZoom(zoom + 0.1);
  $("zoom-out").onclick = () => setZoom(zoom - 0.1);
  $("fit").onclick = fit;
  const viewport = $("viewport");
  let drag;
  viewport.onpointerdown = (e) => {
    if (e.target.closest("button, input, select, textarea, .data-node")) return;
    drag = {
      x: e.clientX,
      y: e.clientY,
      left: viewport.scrollLeft,
      top: viewport.scrollTop,
    };
    viewport.setPointerCapture(e.pointerId);
    viewport.classList.add("dragging");
  };
  viewport.onpointermove = (e) => {
    if (!drag) return;
    viewport.scrollLeft = drag.left - (e.clientX - drag.x);
    viewport.scrollTop = drag.top - (e.clientY - drag.y);
  };
  viewport.onpointerup = viewport.onpointercancel = () => {
    drag = undefined;
    viewport.classList.remove("dragging");
  };
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      connecting = undefined;
      report("");
      $("node-panel").hidden = true;
      renderCanvas();
    }
  });
}

$("run").onclick = () => {
  $("execution").showModal();
  run();
};
setupVisual();
controls();
renderQueue();
await action(async () => {
  show(await call("/workflow"));
  const [canonical, sourceTemplates] = await Promise.all([
    call("/workflows"),
    call("/prototype/templates"),
  ]);
  templates = [
    ...canonical
      .filter((t) => ["deep-research", "weekly-kpis"].includes(t.id))
      .map((t) => ({ ...t, canonical: true })),
    ...sourceTemplates,
  ];
  $("template").replaceChildren(
    ...templates.map((template) => {
      const option = document.createElement("option");
      option.value = template.id;
      option.textContent = template.name;
      option.title = template.description;
      return option;
    }),
  );
  report("");
});

fit();
