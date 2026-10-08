/* The workbench timeline: one keyed projection of a session, patched in place.

   Every row has one key, derived from typed provenance only (a turn id, an
   input id, a call id, a row id), never from display text.
   The live form of a row and its committed form share that key, so a commit
   patches the node the reader is already looking at instead of replacing it.

   Every source upserts into the same store: the optimistic row a send shows,
   the product lane, live observations, the committed rows a turn's commit
   carries, and the authoritative snapshot read at boot or after a replay gap.
   Each row keeps what every source said about it; the row shows the most
   authoritative of them (committed, then product, then live, then optimistic)
   and disappears only when no source holds it any more.

   Rows are ordered by when they happened, in the server's clock: a press at
   its press time, a send at its admission, live activity as it arrives, and a
   committed row the reader never saw live right after the committed row that
   precedes it. A row keeps the position it was first given, so nothing jumps.
   The DOM is cleared only by `reset`, on a session switch. */

// ── presentation helpers, shared with the rest of the page ──────────────────

/* One element, with its class and its text. */
function el(tag, className = "", text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function escapeHtml(value) {
  return String(value || "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

/* Every abbreviated identity in this console keeps its full value in the
   DOM: the title carries it, a copy control puts it on the clipboard, and
   the visible form elides the middle so two ids sharing a prefix (or a
   suffix) stay distinguishable. Nothing is shortened destructively. */
function middleEllipsis(value, max = 30) {
  const text = String(value ?? "");
  if (text.length <= max) return text;
  const head = Math.ceil((max - 1) / 2);
  const tail = max - 1 - head;
  return text.slice(0, head) + "…" + text.slice(text.length - tail);
}

function copyButton(value, label = "copy") {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "copy-btn";
  button.textContent = label;
  button.dataset.copyValue = String(value ?? "");
  button.title = "Copy the full value";
  button.setAttribute("aria-label", "Copy the full value to the clipboard");
  return button;
}

function valueRow(key, value, opts = {}) {
  const row = el("div", "value-row");
  const name = el("span", "value-key", key);
  const text = String(value ?? "");
  const shown = el("span", "value-text", opts.max === 0 ? text : middleEllipsis(text, opts.max || 30));
  shown.title = text;
  row.append(name, shown);
  if (text) row.appendChild(copyButton(text));
  return row;
}

function looksLikeJson(text) {
  const trimmed = String(text || "").trim();
  return (trimmed.startsWith("{") && trimmed.endsWith("}"))
    || (trimmed.startsWith("[") && trimmed.endsWith("]"));
}

function prettyJson(text) {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch (_) {
    return String(text ?? "");
  }
}

function payloadSummaryLabel(kind, body) {
  const lines = body ? body.split("\n").length : 0;
  return kind
    + " · " + (looksLikeJson(body) ? "json" : "text")
    + " · " + body.length + " chars"
    + (lines > 1 ? " · " + lines + " lines" : "");
}

/* Long payloads — event bodies, raw errors — collapse to one
   summary line. The body is a real, selectable, copyable block. */
function payloadDisclosure(kind, text, opts = {}) {
  const body = !opts.verbatim && looksLikeJson(text) ? prettyJson(text) : String(text ?? "");
  const details = el("details", "payload");
  details.open = Boolean(opts.open);
  const summary = el("summary", "", payloadSummaryLabel(kind, body));
  summary.append(copyButton(body));
  const pre = el("pre", "", body);
  details.append(summary, pre);
  return details;
}

function formatClockTime(value) {
  if (!value) return "";
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return "";
  /* 24h, zero-padded: a runtime console reads clocks in columns, and
     "12:40:39 PM" is three characters of furniture per row. */
  return parsed.toLocaleTimeString([], {
    hour12: false,
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit"
  });
}

function timeStamp(value) {
  const shown = formatClockTime(value);
  if (!shown) return null;
  const stamp = el("time", "msg-time");
  stamp.dateTime = String(value);
  stamp.textContent = shown;
  stamp.title = String(value);
  return stamp;
}

/* The one renderer for the system/process lane: a boxed, left-aligned,
   mono row that can never be mistaken for an agent reply. */
function fillEventLane(node, body, kind, text, at) {
  const head = el("div", "event-head");
  const kindLabelNode = el("span", "event-kind");
  const title = el("span", "event-title");
  const raw = String(text ?? "");
  kindLabelNode.textContent = kind || "event";
  title.textContent = raw.split("\n")[0] || "";
  head.append(kindLabelNode, title);
  const stamp = timeStamp(at);
  if (stamp) head.appendChild(stamp);
  body.appendChild(head);
  const rest = raw.split("\n").slice(1).join("\n");
  /* Committed event text is canonical prose, including JSON detail lines.
     Reformatting it would change what the transcript row says. */
  if (rest.trim()) body.appendChild(payloadDisclosure("detail", rest, { verbatim: true }));
}

function renderInlineMarkdown(value) {
  return escapeHtml(value)
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\*([^*]+)\*/g, "<em>$1</em>");
}

function renderMarkdownBlocks(markdown) {
  const lines = String(markdown || "").replace(/\r\n?/g, "\n").split("\n");
  const blocks = [];
  let paragraph = [];
  let list = null;
  let code = null;

  function flushParagraph() {
    if (!paragraph.length) return;
    blocks.push(`<p>${renderInlineMarkdown(paragraph.join(" ").trim())}</p>`);
    paragraph = [];
  }

  function flushList() {
    if (!list) return;
    const items = list.items.map((item) => `<li>${renderInlineMarkdown(item)}</li>`).join("");
    blocks.push(`<${list.tag}>${items}</${list.tag}>`);
    list = null;
  }

  for (const line of lines) {
    const fence = line.match(/^```(\w+)?\s*$/);
    if (code) {
      if (fence) {
        blocks.push(`<pre><code>${escapeHtml(code.lines.join("\n"))}</code></pre>`);
        code = null;
      } else {
        code.lines.push(line);
      }
      continue;
    }
    if (fence) {
      flushParagraph();
      flushList();
      code = { lines: [] };
      continue;
    }
    if (!line.trim()) {
      flushParagraph();
      flushList();
      continue;
    }
    const heading = line.match(/^(#{1,3})\s+(.+)$/);
    if (heading) {
      flushParagraph();
      flushList();
      const level = heading[1].length;
      blocks.push(`<h${level}>${renderInlineMarkdown(heading[2].trim())}</h${level}>`);
      continue;
    }
    const ordered = line.match(/^\s*\d+[.)]\s+(.+)$/);
    const unordered = line.match(/^\s*[-*]\s+(.+)$/);
    if (ordered || unordered) {
      flushParagraph();
      const tag = ordered ? "ol" : "ul";
      if (!list || list.tag !== tag) flushList();
      if (!list) list = { tag, items: [] };
      list.items.push((ordered || unordered)[1].trim());
      continue;
    }
    flushList();
    paragraph.push(line.trim());
  }

  if (code) blocks.push(`<pre><code>${escapeHtml(code.lines.join("\n"))}</code></pre>`);
  flushParagraph();
  flushList();
  return blocks.join("");
}

// BEGIN WORKBENCH_MESSAGE_ATTACHMENTS
function renderMessageAttachments(body, attachments) {
  if (!Array.isArray(attachments) || attachments.length === 0) return null;
  // The server derives every retrieval URL from its stored attachment id;
  // browser clients only render that projection and never supply a URL.
  const gallery = el("div", "message-attachments");
  for (const [index, attachment] of attachments.entries()) {
    const retrieveUrl = String(attachment?.retrieve_url || "").trim();
    if (!retrieveUrl) continue;
    const link = el("a", "message-attachment");
    link.href = retrieveUrl;
    link.target = "_blank";
    link.rel = "noopener";
    link.dataset.attachmentId = String(attachment?.attachment_id || "");
    const image = document.createElement("img");
    image.src = retrieveUrl;
    image.alt = attachments.length === 1
      ? "Uploaded image attachment"
      : `Uploaded image attachment ${index + 1}`;
    const broken = el("span", "attachment-broken");
    broken.hidden = true;
    broken.textContent = "Image unavailable · open original";
    image.addEventListener("error", () => {
      image.hidden = true;
      broken.hidden = false;
    }, { once: true });
    link.append(image, broken);
    gallery.appendChild(link);
  }
  if (gallery.children.length === 0) return null;
  body.appendChild(gallery);
  return gallery;
}
// END WORKBENCH_MESSAGE_ATTACHMENTS

/* A committed row names its attachments by stored id; the page asks the
   workbench for them at the URL the server derives from that id. */
function committedAttachments(content) {
  return (content?.attachments || []).map(attachment => ({
    attachment_id: attachment.id,
    retrieve_url: `/api/attachments/${encodeURIComponent(attachment.id)}`
  }));
}

function cleanErrorText(message) {
  let text = String(message || "request failed").trim();
  if (/<!doctype|<html/i.test(text)) text = "the server returned an error page (not a normal response)";
  return text.length > 280 ? text.slice(0, 280) + "…" : text;
}

function renderTerminalValue(value) {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  return "```json\n" + JSON.stringify(value, null, 2) + "\n```";
}

function formatApprovalAge(ageMs) {
  const seconds = Math.max(0, Math.floor((Number(ageMs) || 0) / 1000));
  if (seconds < 60) return seconds + "s";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return minutes + "m";
  return Math.floor(minutes / 60) + "h";
}

// BEGIN WORKBENCH_TOOL_CODE_PROJECTION
function toolOutcome(event) {
  if (event?.kind === "durable_summary") return { status: event.status };
  return event?.output?.outcome || {};
}

function toolResult(event) {
  const outcome = toolOutcome(event);
  return outcome.payload ?? event?.output?.value ?? event?.result ?? null;
}

function toolSucceeded(event) {
  return toolOutcome(event).status === "success";
}

function toolRunning(event) {
  return event?.type === "tool_call_started" || event?.phase === "running";
}

function cleanArgs(args) {
  const out = { ...(args || {}) };
  delete out.__session_id__;
  return out;
}

function compactToolPayload(event) {
  if (event?.kind === "durable_summary") return { status: event.status };
  const succeeded = toolSucceeded(event);
  const running = toolRunning(event);
  return {
    args: cleanArgs(event.args),
    status: toolOutcome(event).status || (running ? "running" : (succeeded ? "success" : "unknown")),
    ...(succeeded ? { result: toolResult(event) } : {})
  };
}

function displayToolName(name) {
  if (typeof name === "string" && name.startsWith("mcp__parallel__web_search_")) return "web.search";
  if (typeof name === "string" && name.startsWith("mcp__parallel__web_fetch_")) return "web.fetch";
  if (name === "spawn_agent") return "agents.spawn";
  return name || "tool";
}

function summarizeToolResult(name, result) {
  if (typeof name === "string" && name.startsWith("mcp__parallel__web_search_")) {
    const count = Array.isArray(result?.results) ? result.results.length : 0;
    return `Search completed · ${count} result${count === 1 ? "" : "s"}`;
  }
  if (typeof name === "string" && name.startsWith("mcp__parallel__web_fetch_")) {
    const chars = typeof result?.content === "string" ? result.content.length : 0;
    return `Fetched ${result?.url || "URL"} · ${chars} chars`;
  }
  if (name === "spawn_agent") {
    return result?.summary || result?.session_id || "Subagent completed";
  }
  return "Tool completed";
}

/* One tool row, created once and patched as the call runs and completes. */
function createToolView() {
  const node = document.createElement("div");
  const head = el("div", "tool-head");
  const name = document.createElement("strong");
  const badge = el("span", "badge");
  const timing = document.createElement("span");
  head.append(name, badge, timing);
  const summary = el("div", "tool-summary");
  node.append(head, summary);
  return { node, name, badge, timing, summary, payload: null, signature: "" };
}

function patchToolView(view, event) {
  if (event?.kind === "omitted") {
    view.node.className = "tool omitted";
    view.node.replaceChildren(
      `${event.count} earlier tool call${event.count === 1 ? "" : "s"} omitted from durable history`
    );
    return;
  }
  const signature = JSON.stringify(event);
  if (signature === view.signature) return;
  view.signature = signature;
  const durable = event?.kind === "durable_summary";
  const running = toolRunning(event);
  const ok = toolSucceeded(event);
  view.node.className = "tool" + (running ? " pending" : (ok ? "" : " fail"));
  view.name.textContent = durable ? event.operation : displayToolName(event.name);
  view.badge.textContent = running ? "running" : (ok ? "completed" : "failed");
  view.timing.textContent = durable
    ? "durable outcome only"
    : running
    ? "in progress"
    : `${ok ? "ok" : "failed"} in ${event.duration_ms || 0}ms`;
  view.summary.textContent = durable
    ? `Tool ${ok ? "completed" : "failed"} · arguments, result, duration, and call identity unavailable after reload`
    : running
    ? "Tool running"
    : (ok ? summarizeToolResult(event.name, toolResult(event)) : "Tool failed");
  const payload = payloadDisclosure(
    "JSON payload",
    JSON.stringify(compactToolPayload(event), null, 2),
    { open: view.payload ? view.payload.open : false }
  );
  if (view.payload) view.payload.replaceWith(payload);
  else view.node.appendChild(payload);
  view.payload = payload;
}

function codeBlockStateLabel(event) {
  if (event.phase === "running") return "running";
  return event.error ? "failed" : "completed";
}
// END WORKBENCH_TOOL_CODE_PROJECTION

// ── the keyed projection ────────────────────────────────────────────────────

/* How long a turn that reported `done` may wait for its commit before the
   rows it showed live, and that nothing committed, retire. A turn that
   commits nothing (an abort drops uncommitted work) has no commit to wait
   for; one that does commits before its `done` is published. */
const TERMINAL_COMMIT_GRACE_MS = 3000;

/* A turn still counts as running after another turn's `done` only if it
   reported activity this recently. A session runs one turn at a time, so
   the `done` of the run also ends the physical turns it continued into. */
const ACTIVE_TURN_IDLE_MS = 2000;

const SOURCE_PRECEDENCE = ["committed", "product", "pending", "live", "optimistic", "local"];

function parseServerTime(value) {
  const parsed = Date.parse(String(value ?? ""));
  return Number.isFinite(parsed) ? parsed : null;
}

function compareOrder(left, right) {
  return left.t - right.t || left.group - right.group || left.lane - right.lane || left.seq - right.seq;
}

const TURN_LANES = { input: 0, thinking: 1, code: 2, tool: 2, retry: 2, event: 2, reply: 3 };

// BEGIN WORKBENCH_SETTLED_TRANSCRIPT
/* The keys committed rows take, from typed provenance alone. A turn's first
   committed user row is its input (`input:<turn>`), the row its optimistic
   and product forms already hold; another user row with an input id is that
   applied input (`applied:<input>`). The reply is `reply:<turn>`. A turn's
   code blocks and reasoning texts are counted in commit order, exactly as
   the live stream counts them. Everything else is its own row. `seen` keeps
   the key each row was given, so redelivery assigns the same keys. */
function committedRowEntries(rows, seen, counters) {
  const entries = [];
  const counter = turnId => {
    if (!counters.has(turnId)) counters.set(turnId, { input: null, code: 0, thinking: 0 });
    return counters.get(turnId);
  };
  for (const row of rows || []) {
    if (row.suppressed) continue;
    const turnId = row.provenance?.turn_id || null;
    const content = row.content || {};
    let assigned = seen.get(row.row_id);
    if (!assigned) {
      const turn = turnId ? counter(turnId) : null;
      const thinking = content.reasoning.map((_, index) =>
        turn ? `thinking:${turnId}:${turn.thinking++}` : `row:${row.row_id}:reasoning:${index}`);
      let key;
      if (row.kind === "reasoning") key = null;
      else if (!turn) key = `row:${row.row_id}`;
      else if (row.kind === "user" && turn.input === null) {
        turn.input = row.row_id;
        key = `input:${turnId}`;
      } else if (row.kind === "user" && row.provenance.input_id) key = `applied:${row.provenance.input_id}`;
      else if (row.kind === "assistant_reply" && row.provenance.is_turn_reply) key = `reply:${turnId}`;
      else if (row.kind === "code_block") key = `code:${turnId}:${turn.code++}`;
      else key = `row:${row.row_id}`;
      assigned = { thinking, key };
      seen.set(row.row_id, assigned);
    }
    const kind = row.kind === "user" ? "input"
      : row.kind === "assistant_reply" ? "reply"
      : row.kind === "code_block" ? "code"
      : row.kind === "reasoning" ? "thinking"
      : "event";
    /* Reasoning carried by a call or reply belongs beside that row, in
       commit order, rather than ahead of every tool round in the turn. */
    assigned.thinking.forEach((key, index) => entries.push({
      key, kind: "thinking", laneKind: kind, turnId, rowId: row.row_id, row, text: content.reasoning[index]
    }));
    if (assigned.key) {
      entries.push({ key: assigned.key, kind, turnId, rowId: row.row_id, row });
    }
  }
  return entries;
}
// END WORKBENCH_SETTLED_TRANSCRIPT

function createWorkbenchTimeline({ list, footer, empty, hooks = {} }) {
  const rows = new Map();
  let ordered = [];
  const footerRows = new Map();
  const turns = new Map();
  const activeTurns = new Map();
  const appliedObservations = new Set();
  const committedKeys = new Map();
  const committedCounters = new Map();
  const admittedInputs = new Set();
  let pendingSends = 0;
  /* Every change takes the next epoch. A snapshot read that started at
     epoch E retires only what was last said at or before E: anything the
     streams delivered while it was in flight is at least as new as it. */
  let epoch = 0;
  let sequence = 0;
  let localSequence = 0;
  let clockOffset = 0;
  let lastBusy = false;
  const now = () => (hooks.now ? hooks.now() : Date.now());
  const serverNow = () => now() + clockOffset;

  function turn(turnId) {
    if (!turns.has(turnId)) {
      turns.set(turnId, {
        terminal: false,
        failed: false,
        code: 0,
        openCode: null,
        thinking: new Map(),
        graceTimer: 0
      });
    }
    return turns.get(turnId);
  }

  /* The server clock, learned from product rows stamped as they were
     published: arrival is never earlier than publication, so the largest
     difference seen is the closest estimate. Old rows a reconnect replays
     say nothing about the clock and are skipped. */
  function observeServerTime(at) {
    const stamped = parseServerTime(at);
    if (stamped === null) return;
    const offset = stamped - now();
    if (Math.abs(offset) < 60000) clockOffset = Math.max(clockOffset, offset);
  }

  function placeAt(t) {
    const seq = ++sequence;
    return { t, group: seq, lane: 0, seq };
  }

  /* Time orders independent occurrences and turns. Within a turn, input,
     thinking, execution and reply are causal lanes: prose can stream before
     the cell it describes runs, and its node can be recorded before that
     cell's result. Clamp each lane after the lanes preceding it, retaining
     execution order within a lane. Only a new row extends these bounds;
     committing an existing keyed row never changes its place. */
  function placeInTurn(order, kind, turnId) {
    if (!turnId || TURN_LANES[kind] === undefined) return order;
    const siblings = rowsOfTurn(turnId);
    order.lane = TURN_LANES[kind];
    order.group = siblings[0]?.order.group ?? order.group;
    for (const sibling of siblings) {
      if (sibling.order.lane < order.lane) order.t = Math.max(order.t, sibling.order.t);
    }
    let changed = false;
    for (const sibling of siblings) {
      if (sibling.order.lane > order.lane && sibling.order.t < order.t) {
        sibling.order.t = order.t;
        changed = true;
      }
    }
    if (changed) ordered.sort((left, right) => compareOrder(left.order, right.order));
    return order;
  }

  function insertOrdered(row) {
    let low = 0;
    let high = ordered.length;
    while (low < high) {
      const middle = (low + high) >> 1;
      if (compareOrder(ordered[middle].order, row.order) <= 0) low = middle + 1;
      else high = middle;
    }
    ordered.splice(low, 0, row);
  }

  /* The one way a row comes into being or changes: one source's view of it.
     A new row takes `order`; an existing row keeps the place it has. */
  function upsert(key, kind, source, payload, order) {
    let row = rows.get(key);
    if (!row) {
      row = { key, kind, order: placeInTurn(order(), payload?.laneKind || kind, payload?.turnId), sources: {}, stamps: {}, turnId: null, view: null, dirty: true };
      rows.set(key, row);
      insertOrdered(row);
    }
    row.sources[source] = payload;
    row.stamps[source] = ++epoch;
    if (payload?.turnId) row.turnId = payload.turnId;
    row.dirty = true;
    return row;
  }

  function release(key, source) {
    const row = rows.get(key);
    if (!row || !(source in row.sources)) return;
    delete row.sources[source];
    row.dirty = true;
  }

  /* A row whose key was provisional takes its real key in place. Should
     the real key already exist, the provisional row's own view folds into
     it and the provisional row retires: its counterpart now stands for it. */
  function rekey(from, to) {
    const row = rows.get(from);
    const target = rows.get(to);
    if (!row) return target || null;
    if (target) {
      for (const [source, payload] of Object.entries(row.sources)) {
        if (target.sources[source] === undefined) {
          target.sources[source] = payload;
          target.stamps[source] = row.stamps[source];
        }
      }
      target.dirty = true;
      row.sources = {};
      row.dirty = true;
      return target;
    }
    rows.delete(from);
    row.key = to;
    rows.set(to, row);
    row.dirty = true;
    return row;
  }

  function rowsOfTurn(turnId) {
    return ordered.filter(row => row.turnId === turnId);
  }

  /* After a turn's commit and its `done`, the rows only its live stream
     held are what the commit did not keep: they retire. */
  function sweepTurn(turnId) {
    const state = turn(turnId);
    clearTimeout(state.graceTimer);
    state.graceTimer = 0;
    for (const row of rowsOfTurn(turnId)) {
      release(row.key, "live");
      if (state.failed) {
        release(row.key, "product");
        release(row.key, "optimistic");
        release(row.key, "pending");
      }
    }
    state.openCode = null;
  }

  /* `done` ends a turn but deletes nothing. The rows only its live stream
     held retire once its commit is in: at once for a failed turn (it
     commits nothing), when a commit for it arrives after `done`, and
     otherwise after a grace long enough for a commit still in flight. */
  function markTerminal(turnId, failed) {
    const state = turn(turnId);
    state.terminal = true;
    state.terminalStamp = ++epoch;
    state.failed = state.failed || failed;
    activeTurns.delete(turnId);
    const idleSince = now() - ACTIVE_TURN_IDLE_MS;
    for (const [other, lastActivity] of activeTurns) {
      if (lastActivity < idleSince) activeTurns.delete(other);
    }
    if (state.failed) sweepTurn(turnId);
    else if (!state.graceTimer) {
      state.graceTimer = setTimeout(() => {
        state.graceTimer = 0;
        sweepTurn(turnId);
        flush();
      }, TERMINAL_COMMIT_GRACE_MS);
    }
  }

  function markActive(turnId) {
    if (!turnId || turn(turnId).terminal) return;
    activeTurns.set(turnId, now());
    turn(turnId).activeStamp = ++epoch;
  }

  function busy() {
    return pendingSends > 0 || activeTurns.size > 0;
  }

  // ── views: each row's node is created once and patched from its sources ──

  function strongest(row) {
    for (const source of SOURCE_PRECEDENCE) {
      if (row.sources[source] !== undefined) return { source, payload: row.sources[source] };
    }
    return null;
  }

  function stampRow(row, node) {
    const committed = row.sources.committed;
    if (committed?.rowId) {
      node.dataset.transcriptRowId = committed.rowId;
      node.dataset.turnId = committed.turnId || "";
    } else {
      delete node.dataset.transcriptRowId;
      if (row.turnId) node.dataset.turnId = row.turnId;
      else delete node.dataset.turnId;
    }
    node.dataset.key = row.key;
  }

  function messageView(role, label) {
    const node = document.createElement("div");
    node.className = "message " + role;
    const roleNode = el("div", "msg-role");
    const labelNode = el("span", "", label);
    roleNode.appendChild(labelNode);
    const body = el("div", "msg-body");
    const text = el("div", "msg-text");
    body.appendChild(text);
    node.append(roleNode, body);
    return { node, roleNode, body, text, stamp: null, stampValue: "", textValue: null, gallery: null, gallerySignature: "" };
  }

  function patchStamp(view, at) {
    const value = String(at || "");
    if (value === view.stampValue) return;
    view.stampValue = value;
    const stamp = timeStamp(at);
    if (view.stamp) view.stamp.remove();
    view.stamp = stamp;
    if (stamp) view.roleNode.appendChild(stamp);
  }

  function patchAttachments(view, attachments) {
    const signature = JSON.stringify(attachments || []);
    if (signature === view.gallerySignature) return;
    view.gallerySignature = signature;
    if (view.gallery) view.gallery.remove();
    view.gallery = renderMessageAttachments(view.body, attachments);
  }

  function patchText(view, text, markdown) {
    if (text === view.textValue) return;
    view.textValue = text;
    if (markdown) view.text.innerHTML = renderMarkdownBlocks(text);
    else view.text.textContent = text;
  }

  function inputContent(row) {
    const { product, committed, pending, live, optimistic } = row.sources;
    if (product) return { text: product.message.text, attachments: product.message.attachments || [], at: product.message.at };
    if (committed) return { text: committed.row.content.text, attachments: committedAttachments(committed.row.content), at: committed.row.timestamp };
    const provisional = pending || live || optimistic;
    return { text: provisional?.text || "", attachments: provisional?.attachments || [], at: provisional?.at || "" };
  }

  function replyContent(row) {
    const { committed, product, live } = row.sources;
    if (committed) return { text: committed.row.content.text, attachments: committedAttachments(committed.row.content), at: committed.row.timestamp };
    if (product) return { text: product.message.text, attachments: product.message.attachments || [], at: product.message.at };
    return { text: (live?.chunks || []).map(chunk => chunk.text).join(""), attachments: [], at: "" };
  }

  const VIEWS = {
    input: {
      create: () => messageView("user", "you"),
      patch(row, view) {
        const content = inputContent(row);
        patchText(view, content.text, false);
        patchAttachments(view, content.attachments);
        patchStamp(view, content.at);
        const provisional = !row.sources.product && !row.sources.committed;
        view.node.classList.toggle("pending", provisional && !row.sources.optimistic?.failed);
        view.node.classList.toggle("failed", Boolean(row.sources.optimistic?.failed) && provisional);
      }
    },
    reply: {
      create: () => messageView("assistant", "agent"),
      patch(row, view) {
        const content = replyContent(row);
        patchText(view, content.text, true);
        patchAttachments(view, content.attachments);
        patchStamp(view, content.at);
        view.node.hidden = !content.text && !content.attachments.length;
      }
    },
    thinking: {
      create() {
        const node = el("details", "reasoning");
        node.open = true;
        const summary = el("summary", "", "thinking");
        const pre = document.createElement("pre");
        node.append(summary, pre);
        return { node, pre };
      },
      patch(row, view) {
        const text = row.sources.committed
          ? row.sources.committed.text
          : (row.sources.live?.chunks || []).map(chunk => chunk.text).join("");
        if (view.pre.textContent !== text) view.pre.textContent = text;
        view.node.hidden = !text;
      }
    },
    code: {
      create() {
        const node = el("details", "code-block");
        node.open = false;
        const summary = document.createElement("summary");
        const label = document.createElement("span");
        summary.appendChild(label);
        const source = el("pre", "code-source");
        const output = el("pre", "code-output");
        output.hidden = true;
        const tools = el("div", "code-tools");
        node.append(summary, source, output, tools);
        return {
          node, summary, label, source, output, tools, graphKey: "", toolViews: new Map(), liveTools: null, liveEvent: null,
          gallery: null, gallerySignature: "", body: node
        };
      },
      patch(row, view) {
        const live = row.sources.live;
        if (live?.event) view.liveEvent = live.event;
        const committed = row.sources.committed?.row?.content;
        const event = committed
          ? { ...committed, phase: "completed", duration_ms: view.liveEvent?.duration_ms, graph_key: view.liveEvent?.graph_key }
          : (live?.event || { phase: "running" });
        view.node.classList.toggle("fail", Boolean(event.error));
        const output = [event.output, typeof event.error === "string" ? event.error : event.error?.message]
          .filter(Boolean).join("\n");
        if (view.source.textContent !== (event.code || "")) view.source.textContent = event.code || "";
        if (view.output.textContent !== output) view.output.textContent = output;
        view.output.hidden = !output;
        if (committed) patchAttachments(view, committedAttachments(committed));
        /* The live tool rows carry the call's arguments and result; the
           committed summaries only its outcome. A block the reader watched
           keeps its live rows; one first seen committed shows the summaries. */
        if (live?.tools?.length) view.liveTools = live.tools;
        const liveTools = view.liveTools || [];
        const tools = liveTools.length || !committed
          ? liveTools.map(tool => ({ key: `call:${tool.event.call_id}`, event: tool.event }))
          : [
              ...committed.tools.map((tool, index) => ({ key: `durable:${index}`, event: { kind: "durable_summary", ...tool } })),
              ...(committed.tools_omitted ? [{ key: "omitted", event: { kind: "omitted", count: committed.tools_omitted } }] : [])
            ];
        for (const tool of tools) {
          let toolView = view.toolViews.get(tool.key);
          if (!toolView) {
            toolView = createToolView();
            view.toolViews.set(tool.key, toolView);
            view.tools.appendChild(toolView.node);
          }
          patchToolView(toolView, tool.event);
        }
        const omitted = tools.filter(tool => tool.event.kind === "omitted")
          .reduce((total, tool) => total + (Number(tool.event.count) || 0), 0);
        const retained = tools.length - tools.filter(tool => tool.event.kind === "omitted").length;
        const toolCount = tools.length ? retained + omitted : (event.tool_call_ids || []).length;
        const toolLabel = toolCount ? ` · ${toolCount} tool${toolCount === 1 ? "" : "s"}` : "";
        const omittedLabel = omitted ? ` · ${omitted} omitted` : "";
        const durationLabel = event.duration_ms === undefined || event.duration_ms === null ? "" : ` in ${event.duration_ms || 0}ms`;
        view.label.textContent = `${event.language || "code"} ${codeBlockStateLabel(event)}${durationLabel}${toolLabel}${omittedLabel}`;
        if (event.graph_key && event.graph_key !== view.graphKey) {
          view.graphKey = event.graph_key;
          const graphButton = el("button", "work-diagram-button");
          graphButton.type = "button";
          graphButton.title = "open execution graph";
          graphButton.setAttribute("aria-label", "Open execution graph for this code block");
          const icon = el("span", "diagram-button-icon");
          icon.setAttribute("aria-hidden", "true");
          icon.append(document.createElement("span"), document.createElement("span"), document.createElement("span"));
          graphButton.appendChild(icon);
          graphButton.addEventListener("click", click => {
            click.preventDefault();
            click.stopPropagation();
            hooks.openExecutionGraph?.(event.graph_key);
          });
          view.summary.append(" ", graphButton);
        }
      }
    },
    tool: {
      create: () => createToolView(),
      patch(row, view) {
        patchToolView(view, row.sources.live?.event || {});
      }
    },
    event: {
      create() {
        const node = el("div", "message event");
        const body = el("div", "msg-body");
        node.appendChild(body);
        return { node, body, signature: "", gallery: null, gallerySignature: "" };
      },
      patch(row, view) {
        const { committed, product } = row.sources;
        const text = committed ? committed.row.content.text : product?.message.text || "";
        const at = committed ? committed.row.timestamp : product?.message.at;
        const attachments = committed ? committedAttachments(committed.row.content) : product?.message.attachments || [];
        const signature = JSON.stringify([text, at, attachments]);
        if (signature === view.signature) return;
        view.signature = signature;
        view.body.replaceChildren();
        view.gallery = null;
        view.gallerySignature = "";
        fillEventLane(view.node, view.body, "event", text, at);
        patchAttachments(view, attachments);
      }
    },
    retry: {
      create() {
        const node = el("div", "message event retry-status");
        const body = el("div", "msg-body");
        node.appendChild(body);
        return { node, body, signature: "" };
      },
      patch(row, view) {
        const live = row.sources.live;
        view.node.hidden = !live?.event;
        if (!live?.event) return;
        const event = live.event;
        const signature = JSON.stringify(event);
        if (signature === view.signature) return;
        view.signature = signature;
        const wait = event.wait_seconds ? ` · waiting ${event.wait_seconds}s` : "";
        view.body.replaceChildren();
        fillEventLane(view.node, view.body, `provider retry ${event.attempt} of ${event.max_attempts}`,
          `${event.reason || "provider request failed"}${wait}`, live.at);
      }
    },
    note: {
      create() {
        const node = el("div", "note");
        return { node };
      },
      patch(row, view) {
        const payload = strongest(row)?.payload;
        view.node.textContent = payload?.text || "";
      }
    },
    error: {
      create() {
        const node = el("div", "message error");
        const roleNode = el("div", "msg-role", "error");
        const body = el("div", "msg-body");
        node.append(roleNode, body);
        return { node, body, filled: false };
      },
      patch(row, view) {
        if (view.filled) return;
        view.filled = true;
        const payload = row.sources.local;
        const rawError = String(payload.text || "request failed").trim();
        const shownError = cleanErrorText(payload.text);
        view.body.textContent = shownError;
        if (rawError && rawError !== shownError) view.body.appendChild(payloadDisclosure("full error", rawError));
        if (payload.retry) {
          const retry = el("button", "retry");
          retry.type = "button";
          retry.textContent = "retry turn";
          retry.addEventListener("click", () => payload.retry());
          view.body.append(document.createElement("br"), retry);
        }
      }
    },
    approval: {
      create: row => approvalView(row),
      patch: (row, view) => patchApprovalView(row, view)
    }
  };

  /* A pending approval is part of the conversation: one compact card after
     the tool call it waits on (named by that call's id), collapsing to one
     line once decided. */
  function approvalView(row) {
    const node = el("div", "approval-card");
    return { node, filledFor: null, age: null };
  }

  function patchApprovalView(row, view) {
    const { approval, decided } = row.sources.local;
    view.node.dataset.approvalKey = approval.key;
    view.node.dataset.tool = approval.tool;
    if (decided) {
      if (view.filledFor === "decided:" + decided) return;
      view.filledFor = "decided:" + decided;
      view.node.dataset.decided = decided;
      const line = el("span", "approval-line", decided + ": " + approval.tool);
      view.node.replaceChildren(line);
      return;
    }
    if (view.filledFor === "pending") {
      view.age.textContent = "waiting " + formatApprovalAge(approval.age_ms);
      return;
    }
    view.filledFor = "pending";
    const head = el("div", "approval-head");
    const label = el("span", "approval-label", "approval needed");
    const tool = el("strong", "approval-tool", approval.tool);
    view.age = el("span", "approval-age", "waiting " + formatApprovalAge(approval.age_ms));
    const approve = el("button", "approval-approve");
    approve.type = "button";
    approve.textContent = "approve";
    approve.setAttribute("aria-label", "Approve " + approval.tool);
    const deny = el("button", "approval-deny");
    deny.type = "button";
    deny.textContent = "deny";
    deny.setAttribute("aria-label", "Deny " + approval.tool);
    async function decide(decision, button) {
      approve.disabled = true;
      deny.disabled = true;
      button.textContent = decision === "approve" ? "approving" : "denying";
      try {
        await hooks.decideApproval(approval, decision);
        const outcome = decision === "approve" ? "approved" : "denied";
        upsert(row.key, "approval", "local", { approval, decided: outcome }, () => row.order);
        flush();
      } catch (_) {
        approve.disabled = false;
        deny.disabled = false;
        button.textContent = "retry " + decision;
      }
    }
    approve.addEventListener("click", () => decide("approve", approve));
    deny.addEventListener("click", () => decide("deny", deny));
    head.append(label, tool, view.age, approve, deny);
    view.node.replaceChildren(head, payloadDisclosure("arguments", JSON.stringify(approval.arguments, null, 2)));
  }

  // ── footer: input and work still waiting for a turn ──

  function footerRow(key, kind, payload) {
    let entry = footerRows.get(key);
    if (!entry) {
      entry = { key, kind, payload, view: null };
      footerRows.set(key, entry);
    }
    entry.payload = payload;
    entry.dirty = true;
    return entry;
  }

  function renderReceipt(entry) {
    const receipt = entry.payload;
    const scope = receipt.ingress?.scope || receipt.ingress || "next_turn";
    if (!entry.view) {
      const node = document.createElement("div");
      const kind = el("div", "ingress-kind");
      const body = el("div", "ingress-text");
      node.append(kind, body);
      entry.view = { node, kind, body };
    }
    const { node, kind, body } = entry.view;
    node.className = "ingress-receipt " + (scope === "active_turn" ? "active-turn" : "next-turn");
    node.dataset.inputId = receipt.input_id;
    node.dataset.ingress = scope;
    if (receipt.status?.kind) node.dataset.status = receipt.status.kind;
    let label = receipt.applied ? "applied to turn" : scope === "active_turn" ? "injected now" : "queued next";
    if (!receipt.applied && receipt.status?.kind === "held" && receipt.status.shift_epoch != null) {
      node.dataset.shiftEpoch = String(receipt.status.shift_epoch);
      label += ` · held under epoch ${receipt.status.shift_epoch}`;
    }
    kind.textContent = label;
    body.textContent = receipt.text || "input accepted";
  }

  function renderBatch(entry) {
    if (entry.view) return;
    const batch = entry.payload;
    const node = el("div", "message event queued-batch");
    node.dataset.batchId = batch.batch_id;
    const body = el("div", "msg-body");
    const head = el("div", "event-head");
    const kind = el("span", "event-kind", "queued");
    const title = el("span", "event-title");
    const payloadKind = batch.items?.[0]?.payload?.type || "queued_work";
    title.textContent = payloadKind.replaceAll("_", " ") + " · waits for the next turn";
    title.title = batch.batch_id;
    const cancel = el("button", "inline-action");
    cancel.type = "button";
    cancel.textContent = "cancel";
    cancel.title = "Cancel this pending queued-work batch";
    cancel.setAttribute("aria-label", "Cancel queued-work batch " + batch.batch_id);
    cancel.addEventListener("click", async () => {
      cancel.disabled = true;
      cancel.textContent = "cancelling";
      try {
        await hooks.cancelQueuedBatch(batch.batch_id);
      } catch (_) {
        cancel.disabled = false;
        cancel.textContent = "retry cancel";
      }
    });
    head.append(kind, title, cancel);
    body.appendChild(head);
    node.appendChild(body);
    entry.view = { node };
  }

  // ── the DOM patch: create missing nodes, patch changed ones, keep order ──

  function flush() {
    for (const row of [...rows.values()]) {
      if (row.dirty && !strongest(row)) {
        rows.delete(row.key);
        ordered = ordered.filter(candidate => candidate !== row);
        row.view?.node.remove();
      }
    }
    let cursor = list.firstChild;
    for (const row of ordered) {
      if (!row.view) {
        row.view = VIEWS[row.kind].create(row);
        row.dirty = true;
      }
      if (row.dirty) {
        VIEWS[row.kind].patch(row, row.view);
        stampRow(row, row.view.node);
        row.dirty = false;
      }
      if (row.view.node === cursor) cursor = cursor.nextSibling;
      else list.insertBefore(row.view.node, cursor);
    }
    let footerCursor = footer.firstChild;
    for (const entry of footerRows.values()) {
      if (entry.dirty || !entry.view) {
        if (entry.kind === "receipt") renderReceipt(entry);
        else renderBatch(entry);
        entry.view.node.dataset.key = entry.key;
        entry.dirty = false;
      }
      if (entry.view.node === footerCursor) footerCursor = footerCursor.nextSibling;
      else footer.insertBefore(entry.view.node, footerCursor);
    }
    empty.hidden = rows.size > 0 || footerRows.size > 0;
    const nowBusy = busy();
    if (nowBusy !== lastBusy) {
      lastBusy = nowBusy;
      hooks.busyChanged?.(nowBusy);
    }
  }

  function retireFooter(key) {
    const entry = footerRows.get(key);
    if (!entry) return;
    footerRows.delete(key);
    entry.view?.node.remove();
  }

  // ── sources ──

  /* The product lane's rows: the UI-owned input, the live reply, and host
     event rows. Other roles without provenance are not conversation rows of
     this lane. */
  function productEntry(message) {
    const provenance = message?.provenance || {};
    if (message.role === "user" && provenance.kind === "turn_input") {
      return { key: `input:${provenance.turn_id}`, kind: "input", turnId: provenance.turn_id };
    }
    if (message.role === "assistant" && provenance.kind === "turn_output") {
      return { key: `reply:${provenance.turn_id}`, kind: "reply", turnId: provenance.turn_id };
    }
    if (message.role === "event") return { key: `msg:${message.id}`, kind: "event" };
    return null;
  }

  function applyProductMessage(message, live) {
    const entry = productEntry(message);
    if (!entry) return;
    if (live) observeServerTime(message.at);
    if (message.client_nonce && entry.kind === "input") rekey(`input:client:${message.client_nonce}`, entry.key);
    const at = parseServerTime(message.at);
    upsert(entry.key, entry.kind, "product", { message, turnId: entry.turnId || null },
      () => placeAt(at ?? serverNow()));
  }

  function applyReceipt(receipt) {
    if (!receipt?.input_id || admittedInputs.has(receipt.input_id)) return;
    footerRow(`receipt:${receipt.input_id}`, "receipt", { ...footerRows.get(`receipt:${receipt.input_id}`)?.payload, ...receipt });
  }

  /* An admitted input is its run's user row, never a waiting receipt. */
  function admitInput(inputId, runId, text, midTurn) {
    admittedInputs.add(inputId);
    const receipt = footerRows.get(`receipt:${inputId}`)?.payload;
    retireFooter(`receipt:${inputId}`);
    const key = midTurn ? `applied:${inputId}` : `input:${runId}`;
    const shown = text ?? receipt?.text;
    if (shown === undefined || shown === null) return;
    upsert(key, "input", "pending", { text: shown, turnId: runId, at: "" }, () => placeAt(serverNow()));
  }

  /* A live update to a row its commit already holds is stale: the commit
     is the authority for that row. */
  function liveRow(turnId, key, kind, update) {
    const existing = rows.get(key);
    if (existing?.sources.committed) return;
    const payload = update(existing?.sources.live);
    upsert(key, kind, "live", { ...payload, turnId }, () => placeAt(serverNow()));
  }

  function thinkingKey(turnId, correlationId) {
    const state = turn(turnId);
    if (!state.thinking.has(correlationId)) {
      const live = [...state.thinking.values()].filter(key => rows.get(key)?.sources.live?.chunks?.length).length;
      state.thinking.set(correlationId, `thinking:${turnId}:${live}`);
    }
    return state.thinking.get(correlationId);
  }

  function appendChunk(turnId, key, kind, text, correlationId) {
    if (!text) return;
    liveRow(turnId, key, kind, live => ({ chunks: [...(live?.chunks || []), { correlationId, text }] }));
  }

  function retractChunks(turnId, prefix, correlationIds) {
    const targets = new Set(correlationIds || []);
    if (!targets.size) return;
    for (const row of rowsOfTurn(turnId)) {
      if (!row.key.startsWith(prefix) || !row.sources.live?.chunks) continue;
      const chunks = row.sources.live.chunks.filter(chunk => !targets.has(chunk.correlationId));
      upsert(row.key, row.kind, "live", { ...row.sources.live, chunks }, () => row.order);
    }
    if (prefix === "thinking:") {
      const state = turn(turnId);
      for (const id of targets) state.thinking.delete(id);
    }
  }

  function applyTool(turnId, event) {
    const state = turn(turnId);
    if (state.openCode) {
      liveRow(turnId, state.openCode, "code", live => {
        const tools = [...(live?.tools || [])];
        const index = tools.findIndex(tool => tool.event.call_id === event.call_id);
        const next = { event: event.type === "tool_call_started" ? { ...event, phase: "running" } : event };
        if (index >= 0) tools[index] = next;
        else tools.push(next);
        return { ...live, tools };
      });
      return;
    }
    liveRow(turnId, `tool:${turnId}:${event.call_id}`, "tool", () => ({
      event: event.type === "tool_call_started" ? { ...event, phase: "running" } : event
    }));
  }

  function applyActivity(turnId, event) {
    hooks.turnActivity?.(event, turnId);
    if (!turnId) return;
    const state = turn(turnId);
    if (event.type === "queued_input_accepted") {
      for (const application of event.applications || []) {
        if (!application?.input_id) continue;
        admitInput(application.input_id, application.turn_id || turnId, null, Boolean(application.checkpoint));
      }
    }
    if (state.terminal) return;
    markActive(turnId);
    if (event.type === "model_request_started" && rows.has(`retry:${turnId}`)) {
      liveRow(turnId, `retry:${turnId}`, "retry", () => ({ event: null }));
    }
    const delta = event.type === "stream_block" && event.phase === "delta" ? event.kind : null;
    if (delta === "assistant_text") appendChunk(turnId, `reply:${turnId}`, "reply", event.text, event.correlation_id);
    if (event.type === "final_value" || event.type === "tool_value") {
      appendChunk(turnId, `reply:${turnId}`, "reply", renderTerminalValue(event.value), null);
    }
    if (delta === "reasoning") {
      appendChunk(turnId, thinkingKey(turnId, event.correlation_id), "thinking", event.text, event.correlation_id);
    }
    if (event.type === "model_attempt_reset") {
      retractChunks(turnId, `reply:`, event.assistant_prose_correlation_ids);
      retractChunks(turnId, `thinking:`, event.reasoning_correlation_ids);
    }
    if (event.type === "retry_status") {
      liveRow(turnId, `retry:${turnId}`, "retry", () => ({ event, at: new Date(serverNow()).toISOString() }));
    }
    if (event.type === "code_block_started") {
      const key = `code:${turnId}:${state.code++}`;
      state.openCode = key;
      liveRow(turnId, key, "code", () => ({ event: { ...event, phase: "running" }, tools: [] }));
      if (event.graph_key) hooks.executionStarted?.(event.graph_key);
    }
    if (event.type === "code_block_completed") {
      const key = state.openCode || `code:${turnId}:${state.code++}`;
      state.openCode = null;
      liveRow(turnId, key, "code", live => ({
        tools: live?.tools || [],
        event: { ...event, code: event.code || live?.event?.code || "" }
      }));
    }
    if (event.type === "tool_call_started" || event.type === "tool_call_completed") applyTool(turnId, event);
  }

  /* A commit's rows: each upserts the row its key names. A row the reader
     never saw live goes right after the committed row before it. */
  function applyCommittedRows(rowsCommitted, committedTurnId) {
    let previous = null;
    for (const entry of committedRowEntries(rowsCommitted, committedKeys, committedCounters)) {
      const placeAfter = previous;
      const row = upsert(entry.key, entry.kind, "committed", entry, () => placeAfter
        ? placeAt(placeAfter.order.t)
        : placeAt(parseServerTime(entry.row.timestamp) ?? serverNow()));
      if (entry.row.provenance?.input_id) {
        admittedInputs.add(entry.row.provenance.input_id);
        retireFooter(`receipt:${entry.row.provenance.input_id}`);
      }
      previous = row;
    }
    const committedTurns = new Set(rowsCommitted.map(row => row.provenance?.turn_id).filter(Boolean));
    if (committedTurnId) committedTurns.add(committedTurnId);
    for (const turnId of committedTurns) {
      if (turn(turnId).terminal) sweepTurn(turnId);
    }
  }

  /* The authoritative read (boot, a replay gap, a lagged product lane): it
     replaces what the committed and product sources say, through the same
     upserts, and retires only what it is authoritative for. Live rows of a
     turn it still reports as running stay; nothing is cleared and rebuilt. */
  function applySnapshot(state, since = Infinity) {
    const snapshotNow = serverNow();
    const stale = (row, source) => row.sources[source] !== undefined && row.stamps[source] <= since;
    const committedEntries = committedRowEntries(state.transcript || [], committedKeys, committedCounters);
    const committedByKey = new Map(committedEntries.map(entry => [entry.key, entry]));
    const productByKey = new Map();
    const terminalTurns = new Set();
    for (const event of state.product_events?.events || []) {
      if (event.type === "message") {
        const entry = productEntry(event.message);
        if (entry) productByKey.set(entry.key, { entry, message: event.message });
      }
      if (event.type === "done" && event.turn_id) terminalTurns.add(event.turn_id);
    }
    const running = new Set((state.active_turns || []).map(active => active.turn_id).filter(Boolean));
    const pendingByKey = new Map();
    const waitingReceipts = new Map();
    for (const pending of state.pending_turn_inputs || []) {
      const admission = pending?.input;
      if (!admission?.input_id) continue;
      const text = (admission.input?.items || []).find(item => item?.type === "text" && typeof item.text === "string")?.text
        || "pending input";
      if (pending.status?.kind === "admitted" && pending.status.run) {
        pendingByKey.set(`input:${pending.status.run}`, { text, turnId: pending.status.run, at: "" });
      } else {
        waitingReceipts.set(`receipt:${admission.input_id}`, {
          input_id: admission.input_id, ingress: admission.ingress, state: admission.state, status: pending.status, text
        });
      }
    }
    for (const application of state.turn_input_applications || []) {
      const receipt = waitingReceipts.get(`receipt:${application?.input_id}`);
      if (receipt) receipt.applied = true;
    }

    for (const row of [...rows.values()]) {
      if (!committedByKey.has(row.key) && stale(row, "committed")) release(row.key, "committed");
      if (!productByKey.has(row.key) && stale(row, "product")) release(row.key, "product");
      if (!pendingByKey.has(row.key) && stale(row, "pending")) release(row.key, "pending");
      if (row.turnId && !running.has(row.turnId) && stale(row, "live")) release(row.key, "live");
      const optimistic = row.sources.optimistic;
      if (optimistic && !optimistic.sending && !optimistic.failed && !running.has(row.turnId) && stale(row, "optimistic")) {
        release(row.key, "optimistic");
      }
    }
    let previous = null;
    for (const entry of committedEntries) {
      const placeAfter = previous;
      previous = upsert(entry.key, entry.kind, "committed", entry, () => placeAfter
        ? placeAt(Math.max(placeAfter.order.t, parseServerTime(entry.row.timestamp) ?? placeAfter.order.t))
        : placeAt(parseServerTime(entry.row.timestamp) ?? snapshotNow));
    }
    for (const { message } of productByKey.values()) applyProductMessage(message, false);
    for (const [key, payload] of pendingByKey) upsert(key, "input", "pending", payload, () => placeAt(snapshotNow));
    for (const key of [...footerRows.keys()]) {
      if (footerRows.get(key).kind === "receipt" && !waitingReceipts.has(key)) retireFooter(key);
    }
    for (const [key, receipt] of waitingReceipts) footerRow(key, "receipt", receipt);

    for (const [turnId, known] of turns) {
      const terminalSince = known.terminal && known.terminalStamp > since;
      if (running.has(turnId) && !terminalSince) known.terminal = false;
      if (!running.has(turnId) && (terminalTurns.has(turnId) || committedEntries.some(entry => entry.turnId === turnId))) {
        known.terminal = true;
      }
    }
    for (const [turnId] of [...activeTurns]) {
      if (!running.has(turnId) && !(turn(turnId).activeStamp > since)) activeTurns.delete(turnId);
    }
    for (const turnId of running) {
      if (!turn(turnId).terminal) activeTurns.set(turnId, now());
    }
    flush();
  }

  /* Send is immediate: the row shows and the page is busy before the
     request leaves. The server echoes `nonce` on the row it publishes, and
     the accepted response names the turn, so either one re-keys this row to
     `input:<turn>` in place. */
  function beginSend({ nonce, text, attachments }) {
    pendingSends += 1;
    upsert(`input:client:${nonce}`, "input", "optimistic",
      { text, attachments: attachments || [], at: new Date(serverNow()).toISOString(), sending: true },
      () => placeAt(serverNow()));
    flush();
  }

  function sendAccepted(nonce, accepted) {
    pendingSends = Math.max(0, pendingSends - 1);
    const key = `input:client:${nonce}`;
    if (accepted?.queued) {
      release(key, "optimistic");
      if (accepted.queued_input) applyReceipt(accepted.queued_input);
    } else if (accepted?.turn_id) {
      const row = rekey(key, `input:${accepted.turn_id}`) || rows.get(key);
      if (row?.sources.optimistic) {
        row.sources.optimistic = { ...row.sources.optimistic, sending: false, turnId: accepted.turn_id };
        row.turnId = accepted.turn_id;
        row.dirty = true;
      }
      markActive(accepted.turn_id);
    } else {
      release(key, "optimistic");
    }
    flush();
  }

  function sendFailed(nonce) {
    pendingSends = Math.max(0, pendingSends - 1);
    const row = rows.get(`input:client:${nonce}`);
    if (row?.sources.optimistic) {
      row.sources.optimistic = { ...row.sources.optimistic, sending: false, failed: true };
      row.dirty = true;
    }
    flush();
  }

  function local(kind, payload) {
    upsert(`local:${++localSequence}`, kind, "local", payload, () => placeAt(serverNow()));
    flush();
  }

  function anchorOfCall(callId) {
    if (!callId) return null;
    return ordered.find(row => row.key.endsWith(`:${callId}`) && row.kind === "tool")
      || ordered.find(row => row.kind === "code" && (row.sources.live?.tools || []).some(tool => tool.event.call_id === callId))
      || null;
  }

  function setApprovals(approvals, shownSessionId) {
    const pending = (approvals || []).filter(approval => approval.requesting_session === shownSessionId);
    const keys = new Set(pending.map(approval => `approval:${approval.key}`));
    for (const row of ordered) {
      if (row.kind !== "approval" || keys.has(row.key) || row.sources.local.decided) continue;
      upsert(row.key, "approval", "local", { ...row.sources.local, decided: "resolved" }, () => row.order);
    }
    for (const approval of pending) {
      const key = `approval:${approval.key}`;
      const existing = rows.get(key);
      if (existing?.sources.local.decided) continue;
      const anchor = anchorOfCall(approval.call_id);
      upsert(key, "approval", "local", { approval, decided: null, turnId: anchor?.turnId }, () => {
        return anchor
          ? { ...anchor.order, seq: anchor.order.seq + 0.5 }
          : placeAt(Number(approval.requested_at_ms) || serverNow());
      });
    }
    flush();
  }

  /* Pending queued-work batches wait for the session's next turn boundary,
     so they sit in the footer, one compact row each, and leave it when the
     engine takes them. */
  function setQueuedWork(batches) {
    const pending = new Set((batches || []).map(batch => `batch:${batch.batch_id}`));
    for (const [key, entry] of [...footerRows]) {
      if (entry.kind === "batch" && !pending.has(key)) retireFooter(key);
    }
    for (const batch of batches || []) {
      if (!footerRows.has(`batch:${batch.batch_id}`)) footerRow(`batch:${batch.batch_id}`, "batch", batch);
    }
    flush();
  }

  /* The one clear: a different session, or a cold boot. */
  function reset() {
    for (const state of turns.values()) clearTimeout(state.graceTimer);
    rows.clear();
    ordered = [];
    footerRows.clear();
    turns.clear();
    activeTurns.clear();
    appliedObservations.clear();
    committedKeys.clear();
    committedCounters.clear();
    admittedInputs.clear();
    pendingSends = 0;
    list.replaceChildren();
    footer.replaceChildren();
    flush();
  }

  return {
    reset,
    applySnapshot,
    applyProductEvent(event) {
      if (!event) return;
      if (event.type === "message") applyProductMessage(event.message, true);
      if (event.type === "turn_input") applyReceipt(event.receipt);
      if (event.type === "done" && event.turn_id) markTerminal(event.turn_id, event.outcome === "failed");
      flush();
    },
    /* Observations are at-least-once: each is applied once, by identity,
       for the life of the session. */
    applyObservation(event) {
      const identity = `${event.session_id || ""}:${event.replay_incarnation_id || ""}:${event.cursor || ""}`;
      if (appliedObservations.has(identity)) return;
      appliedObservations.add(identity);
      if (event.type === "turn_activity") applyActivity(event.turn_id || null, event.activity || {});
      if (event.type === "committed") applyCommittedRows(event.rows || [], event.turn_id || null);
      flush();
    },
    showReceipt(receipt) {
      applyReceipt(receipt);
      flush();
    },
    beginSend,
    sendAccepted,
    sendFailed,
    note: text => local("note", { text }),
    error: (text, opts = {}) => local("error", { text, retry: opts.retry || null }),
    setApprovals,
    setQueuedWork,
    busy,
    isTerminal: turnId => Boolean(turnId && turns.get(turnId)?.terminal),
    epoch: () => epoch,
    rowKeys: () => ordered.map(row => row.key),
    nodeOf: key => rows.get(key)?.view?.node || null
  };
}
