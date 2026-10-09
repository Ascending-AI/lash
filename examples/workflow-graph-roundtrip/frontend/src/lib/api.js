// Thin client for the workflow-graph-roundtrip backend contract.
// URLs are relative so the same code works behind the Vite dev proxy and when
// served single-origin from `frontend/dist/`.

// @ts-check

/** @typedef {import('../generated/workflow-document').WorkflowDocument} WorkflowDocument */
/** @typedef {import('../generated/error-response').ErrorBody} ErrorBody */

/** @returns {Promise<WorkflowDocument>} */
export async function fetchWorkflow() {
  const res = await fetch('/workflow', { headers: { accept: 'application/json' } });
  if (!res.ok) throw new Error(`GET /workflow failed: ${res.status}`);
  return /** @type {Promise<WorkflowDocument>} */ (res.json());
}

// Built-in workflow catalog: [{ id, name, description }] in display order.
export async function fetchWorkflows() {
  const res = await fetch('/workflows', { headers: { accept: 'application/json' } });
  if (!res.ok) throw new Error(`GET /workflows failed: ${res.status}`);
  return res.json();
}

// Discards any draft.
export async function selectWorkflow(/** @type {string} */ id) {
  const res = await fetch('/workflow/select', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ id }),
  });
  if (!res.ok) throw new Error(`POST /workflow/select failed: ${res.status}`);
  return res.json();
}

// Save explicit operations, or import source by request intent. The response's
// idMap carries draft correspondence, including request-local insertion ids.
export async function saveWorkflow(/** @type {unknown} */ request) {
  const res = await fetch('/workflow', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(request),
  });
  if (res.ok) {
    const { idMap, ...document } = await res.json();
    return { ok: true, document, idMap };
  }
  /** @type {ErrorBody | null} */
  let body = null;
  try {
    body = /** @type {ErrorBody} */ (await res.json());
  } catch {
    body = null;
  }
  return { ok: false, status: res.status, error: body?.error ?? null };
}

// The saved workflow as Lash's typed document, for the generic structured
// editor: `{ version, graph, nodes: { [id]: { statement, slots } } }`. Each slot
// is `{ path, variant, expression }`; `path` is what a `replaceExpression` edit
// takes as `slot`. No source text is involved.
export async function fetchWorkflowIr() {
  const res = await fetch('/workflow/ir', { headers: { accept: 'application/json' } });
  if (!res.ok) throw new Error(`GET /workflow/ir failed: ${res.status}`);
  return res.json();
}

// Apply typed edits to the saved workflow as one transaction and publish the
// result. `edits` is a list of `{ op, ... }` operations whose content is Lash
// IR as JSON. Resolves like `saveWorkflow`.
export async function applyEdits(/** @type {number} */ version, /** @type {unknown[]} */ edits) {
  const res = await fetch('/workflow/edits', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ version, edits }),
  });
  if (res.ok) {
    const { idMap, ...document } = await res.json();
    return { ok: true, document, idMap: idMap ?? null };
  }
  let error = null;
  try {
    error = (await res.json())?.error ?? null;
  } catch {
    error = { code: `http_${res.status}`, message: await res.text().catch(() => '') };
  }
  return { ok: false, status: res.status, error };
}

// Operation catalog — the sole data home for the "+ Add node" palette. Returns
// an array of catalog entries `[{ id, label, nodeKind, subkind?, operation?,
// effect?, terminalKind?, fields:[{name,type,default}] }]`, or `null` when the
// backend does not serve `/operations` (older build). The caller surfaces a
// "catalog unavailable" state in that case — there is no built-in fallback.
export async function fetchOperations() {
  try {
    const res = await fetch('/operations', { headers: { accept: 'application/json' } });
    if (!res.ok) return null;
    const body = await res.json();
    return Array.isArray(body) ? body : null;
  } catch {
    return null;
  }
}

// `kind` is `expression` | `assignment_target` | `identifier`.
// A missing endpoint (older backend) or any transport failure resolves to `{ ok:true,
// unsupported:true }` so the UI degrades to "no inline verdict" rather than showing false
// errors.
// `availableVars` is the scope the fragment is typed in: TypeScript rejects a fragment that
// reads a name it cannot see, so the field sends the names the node was projected with.
export async function validateFragment(
  /** @type {string} */ kind,
  /** @type {string} */ text,
  /** @type {string[]} */ availableVars = [],
) {
  try {
    const res = await fetch('/validate', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ kind, text, availableVars }),
    });
    if (!res.ok) return { ok: true, unsupported: true };
    const body = await res.json();
    if (body && typeof body.ok === 'boolean') return body;
    return { ok: true, unsupported: true };
  } catch {
    return { ok: true, unsupported: true };
  }
}

// Project canonical source text into a WorkflowDocument (text→graph) for the
// editable source pane. Returns `{ ok:true, document }`, a typed
// `{ ok:false, status, error }` on a 4xx parse error, or
// `{ ok:false, unsupported:true }` when the backend has no `/project` route so
// the pane stays read-only rather than erroring.
export async function projectSource(/** @type {string} */ source) {
  let res;
  try {
    res = await fetch('/project', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ source }),
    });
  } catch (err) {
    return {
      ok: false,
      error: { code: 'network', message: err instanceof Error ? err.message : String(err) },
    };
  }
  if (res.ok) {
    const body = await res.json();
    const document = body?.document ?? body;
    return { ok: true, document };
  }
  if (res.status === 404) return { ok: false, unsupported: true };
  let error = null;
  try {
    error = (await res.json())?.error ?? null;
  } catch {
    error = null;
  }
  return { ok: false, status: res.status, error };
}

// Each call is a brand-new run/invocation.
// `signal` stops observation; the durable process continues.
export async function* runWorkflow(/** @type {AbortSignal} */ signal) {
  const res = await fetch('/run', { method: 'POST', signal });
  if (!res.ok) {
    let detail = '';
    try {
      detail = JSON.stringify(await res.json());
    } catch {
      detail = await res.text().catch(() => '');
    }
    throw new Error(`POST /run failed: ${res.status} ${detail}`);
  }
  if (!res.body) throw new Error('POST /run returned no response body');
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    // SSE frames are separated by a blank line.
    let sep;
    while ((sep = buffer.indexOf('\n\n')) !== -1) {
      const frame = buffer.slice(0, sep);
      buffer = buffer.slice(sep + 2);
      const event = parseFrame(frame);
      if (event) yield event;
    }
  }
}

function parseFrame(/** @type {string} */ frame) {
  const lines = frame.split('\n');
  if (frame.includes("event: run_error")) throw new Error(frame);
  let dataLine = null;
  let eventName = null;
  for (const line of lines) {
    if (line.startsWith('data:')) dataLine = line.slice(5).trim();
    else if (line.startsWith('event:')) eventName = line.slice(6).trim();
  }
  if (eventName === 'keep-alive' || !dataLine) return null;
  try {
    return JSON.parse(dataLine);
  } catch {
    return null;
  }
}

export async function resolveApproval(/** @type {string} */ key, /** @type {unknown} */ payload) {
  const response = await fetch(`/approvals/${encodeURIComponent(key)}`, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload),
  });
  if (!response.ok) throw new Error(await response.text());
}
