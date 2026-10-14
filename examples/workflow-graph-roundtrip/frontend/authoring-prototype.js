import {
  actionOf,
  recordInput as canonicalRecordInput,
  literal,
  conditionValue,
  project as projectScope,
  startPosition as scopeStart,
  appendPosition,
} from "./authoring-projection.mjs";
const $ = (id) => document.getElementById(id);
const key = (site) => JSON.stringify([site.unit, site.path ?? []]);
let workflow,
  templates = [],
  calls = new Map(),
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
  workflow = next;
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
function expression(value) {
  if (value === null) return { literal: "null" };
  if (typeof value === "string") return { literal: { text: value } };
  if (typeof value === "boolean") return { literal: { bool: value } };
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value))
      throw Error(
        "JSON number inputs must be safe integers. Use kernel expression mode for exact floats.",
      );
    return { literal: { int: String(value) } };
  }
  if (Array.isArray(value)) return { list: value.map(expression) };
  return {
    record: Object.entries(value).map(([field, value]) => ({
      field,
      value: expression(value),
    })),
  };
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
  if (selected) renderCallData(selected);
  renderCanvas();
  $("run-output").replaceChildren();
  $("run-status").textContent = "Running";
  const version = workflow.version;
  let sawTerminal = false;
  function observe(event) {
    if (event.workflowVersion !== version) {
      output("Ignored an event for a different workflow revision.", true);
      return;
    }
    if (event.site) statuses.set(key(event.site), event.status);
    if (event.site && event.callId) {
      const site = key(event.site);
      const occurrences = calls.get(site) ?? new Map();
      occurrences.set(event.callId, event);
      calls.set(site, occurrences);
      const node = visualNodes.find(
        (node) => node.s?.action && key(node.s.action) === site,
      );
      if (node) {
        const button = el(
          "button",
          "trace-link",
          `${node.info.name} · ${event.status} · View data`,
        );
        button.onclick = () => {
          $("execution").close();
          openNode(node);
        };
        $("run-output").append(button);
      }
      if (selected?.action && key(selected.action) === site)
        renderCallData(selected);
    }
    if (!event.site) {
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
    const response = await fetch("/run", { method: "POST" });
    if (!response.ok) throw (await response.json()).error;
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
    $("run-status").textContent = "Failed";
    output(errorText(error), true);
  } finally {
    running = false;
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
  return activeScope ? projectScope(workflow, activeScope, model) : [];
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
function wire(from, to, label) {
  const ns = "http://www.w3.org/2000/svg";
  const p = document.createElementNS(ns, "path");
  p.setAttribute(
    "d",
    `M ${from.x} ${from.y} C ${from.x + 65} ${from.y}, ${to.x - 65} ${to.y}, ${to.x} ${to.y}`,
  );
  p.setAttribute("class", "wire");
  p.setAttribute("marker-end", "url(#arrow)");
  $("wires").append(p);
  if (label) {
    const text = document.createElementNS(ns, "text");
    text.setAttribute("x", from.x + 22);
    text.setAttribute("y", from.y + (to.y < from.y ? -14 : 24));
    text.setAttribute("class", "wire-label");
    text.textContent = label;
    $("wires").append(text);
  }
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
function endPoint(x, y) {
  return { x: x + 112, y };
}
function drawNode(node, x, y) {
  const button = el("button", "workflow-node" + (node.start ? " start" : ""));
  button.style.left = x + "px";
  button.style.top = y - 56 + "px";
  button.setAttribute("aria-label", node.title);
  button.append(icon(node.info.icon));
  if (
    node.s &&
    selected &&
    key(selected.site) === key(node.s.site) &&
    !$("node-panel").hidden
  )
    button.classList.add("selected");
  const state = node.s?.action && statuses.get(key(node.s.action));
  if (state) {
    button.classList.add(state);
    if (state === "succeeded") button.append(el("span", "node-status", "✓"));
  }
  const title = el("span", "node-title", node.title);
  title.append(el("span", "node-description", node.subtitle));
  button.append(title, el("span", "port"));
  if (!node.start) button.append(el("span", "port input"));
  button.onclick = () =>
    node.start ? openCatalog(startPosition()) : openNode(node);
  $("flow").append(button);
  visualNodes.push({ ...node, x, y });
  if (!node.branch) {
    wire(endPoint(x, y), { x: x + 164, y });
    addPort(
      x + 164,
      y,
      node.start ? startPosition() : positionAfter(node.s),
      `Add step after ${node.title}`,
    );
  }
  return { x: x + 112, y };
}
function height(nodes) {
  return Math.max(
    1,
    ...nodes.map((n) =>
      n.branch ? height(n.children[0]) + height(n.children[1]) : 1,
    ),
  );
}
function layout(nodes, x, y, incoming) {
  let ends = incoming;
  for (const node of nodes) {
    const end = drawNode(node, x, y);
    for (const prior of ends) wire(prior, { x, y }, prior.label);
    if (node.branch) {
      const a = height(node.children[0]),
        b = height(node.children[1]);
      const top = y - b * 115,
        bottom = y + a * 115;
      const outputs = [];
      for (const [i, branch] of node.children.entries()) {
        const row = i ? bottom : top;
        const source = { ...end, label: i ? "False" : "True" };
        if (branch.length)
          outputs.push(...layout(branch, x + 250, row, [source]));
        else {
          const target = { x: x + 200, y: row };
          wire(source, target, source.label);
          const block = {
            unit: node.s.site.unit,
            path: [...node.s.site.path, i ? 2 : 1],
          };
          if (i === 0 || node.s.statement.if.else_block)
            addPort(
              target.x,
              row,
              { block },
              `Add step to ${i ? "false" : "true"} branch`,
            );
          outputs.push(target);
        }
      }
      ends = outputs;
      x = Math.max(...outputs.map((p) => p.x)) + 140;
    } else {
      ends = [end];
      x += 250;
    }
  }
  return ends;
}
function renderCanvas() {
  if (!workflow) return;
  visualNodes = [];
  $("flow").replaceChildren();
  $("wires").innerHTML =
    '<defs><marker id="arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M0 0 8 4 0 8Z" fill="#9da5aa"/></marker></defs>';
  const roots = project(),
    center = Math.max(290, height(roots) * 115 + 100);
  const start = drawNode(
    {
      start: true,
      info: { icon: "start" },
      title: activeScope?.path?.length ? "Block entry" : "Manual trigger",
      subtitle: activeScope?.path?.length
        ? "Inspecting a nested block"
        : "When you click Run workflow",
    },
    110,
    center,
  );
  layout(roots, 360, center, [start]);
  graphWidth = Math.max(900, ...visualNodes.map((n) => n.x + 290));
  graphHeight = Math.max(600, ...visualNodes.map((n) => n.y + 210));
  $("scene").style.width = graphWidth + "px";
  $("scene").style.height = graphHeight + "px";
  $("node-count").textContent =
    `${visualNodes.length} steps · Click a step to configure it`;
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
function expressionLabel(value) {
  if (value?.literal === "null") return "null";
  const fixed = literal(value);
  if (fixed !== undefined) return JSON.stringify(fixed);
  if (value?.variable) return value.variable;
  const field = value?.member?.field;
  if (field) return `${expressionLabel(field.target)}.${field.field}`;
  const index = value?.member?.index;
  if (index)
    return `${expressionLabel(index.target)}[${expressionLabel(index.index)}]`;
  return "Computed expression · inspect in Advanced";
}
function renderMappings(container, statement) {
  const section = el("section", "input-mappings");
  section.append(el("h3", null, "Input mappings"));
  const input = recordInput(statement);
  const entries =
    input?.record ??
    actionOf(statement).perform.args.map((value, i) => ({
      field: `Input ${i + 1}`,
      value,
    }));
  for (const { field, value } of entries) {
    const row = el("div", "mapping-row");
    row.append(
      el("span", "mapping-key", field),
      el("span", "mapping-value", expressionLabel(value)),
    );
    section.append(row);
  }
  const binding = statement.statement.let?.name;
  if (binding) section.append(el("p", "help", `Output saved as ${binding}`));
  container.append(section);
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
        running
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
      body.append(el("p", "help", "This call's data is no longer available."));
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
function openNode(node) {
  if (busy) return;
  selected = node.s;
  renderPlacement();
  renderSelection();
  const container = panel("STEP SETTINGS");
  identity(container, node.info);
  if (actionOf(node.s)?.perform) {
    renderMappings(container, node.s);
    const data = el("section", "call-data");
    data.id = "call-data";
    container.append(data);
    renderCallData(node.s);
  }
  if (node.generic) {
    container.append(
      el(
        "p",
        "field-note",
        "This is a canonical action or control block. Its inputs and behavior are available in Advanced. Nested blocks can be selected from the scope menu.",
      ),
    );
    const open = el("button", "wide", "Open in Advanced");
    open.onclick = () => {
      $("advanced").showModal();
      $("replace").click();
      $("direct-details").open = true;
    };
    container.append(open);
    renderCanvas();
    controls();
    return;
  }
  if (node.branch) {
    const label = el("label", null, "Condition value");
    label.htmlFor = "branch-value";
    const select = el("select");
    select.id = "branch-value";
    for (const [value, text] of [
      ["true", "Yes"],
      ["false", "No"],
    ]) {
      const o = el("option", null, text);
      o.value = value;
      select.append(o);
    }
    const value = conditionValue(workflow, node.s);
    if (typeof value !== "boolean") {
      container.append(
        el(
          "p",
          "field-note",
          "This condition depends on earlier steps. Use Advanced to edit its rule.",
        ),
      );
      renderCanvas();
      return;
    }
    select.value = value === false ? "false" : "true";
    container.append(
      label,
      select,
      el("p", "help", "Yes follows the True connection. No follows False."),
    );
    const save = el("button", "primary save-node", "Save changes");
    save.onclick = () =>
      action(() =>
        applyVisual([
          {
            direct: {
              set_condition: {
                statement: node.s.site,
                condition: expression(select.value === "true"),
              },
            },
          },
        ]),
      );
    container.append(save);
  } else {
    const input = recordInput(node.s);
    if (input && node.info.fields) {
      const values = Object.fromEntries(
        input.record.map((f) => [f.field, literal(f.value)]),
      );
      const computed = input.record.some((f) => literal(f.value) === undefined);
      if (computed) {
        container.append(
          el(
            "p",
            "field-note",
            "This step uses values from earlier steps. Use Advanced to edit those connections.",
          ),
        );
      } else {
        const read = fields(container, node.info, values);
        const save = el("button", "primary save-node", "Save changes");
        save.onclick = () =>
          action(async () => {
            const withStmt = structuredClone(input.statement.statement);
            withStmt.let.value.expr = expression(read());
            await applyVisual([
              {
                direct: {
                  replace_statement: {
                    statement: input.statement.site,
                    with: withStmt,
                  },
                },
              },
            ]);
          });
        container.append(save);
      }
    } else
      container.append(
        el(
          "p",
          "field-note",
          "This step uses a computed input. Its connections are available in Advanced.",
        ),
      );
    const remove = el("button", "danger save-node", "Delete step");
    remove.onclick = () =>
      action(() =>
        applyVisual([
          { direct: { remove_statement: { statement: node.s.site } } },
        ]),
      );
    container.append(remove);
  }
  container.append(
    el("p", "help", "Changes are saved to this workflow immediately."),
  );
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
    if (e.target.closest("button")) return;
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
