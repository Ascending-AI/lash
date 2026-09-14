// Thin client for the workflow-graph-roundtrip backend contract.
// URLs are relative so the same code works behind the Vite dev proxy and when
// served single-origin from `frontend/dist/`.

export async function fetchWorkflow() {
  const res = await fetch('/workflow', { headers: { accept: 'application/json' } });
  if (!res.ok) throw new Error(`GET /workflow failed: ${res.status}`);
  return res.json();
}

// Built-in workflow catalog: [{ id, name, description }] in display order.
export async function fetchWorkflows() {
  const res = await fetch('/workflows', { headers: { accept: 'application/json' } });
  if (!res.ok) throw new Error(`GET /workflows failed: ${res.status}`);
  return res.json();
}

// Reset the current workflow to a built-in example. Returns its WorkflowDocument
// (same shape as GET /workflow), advancing the version. Discards any draft.
export async function selectWorkflow(id) {
  const res = await fetch('/workflow/select', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ id }),
  });
  if (!res.ok) throw new Error(`POST /workflow/select failed: ${res.status}`);
  return res.json();
}

// Returns { ok: true, document, idMap } on success, or { ok: false, status,
// error } carrying the typed render error so the UI can show it without losing
// the draft. `idMap` ({ "<oldId>": "<newId>", ... }, one entry per posted node)
// lets the caller migrate id-keyed sidecars (positions, selection) across the
// id remint that every Save performs. It arrives either as a sibling key on the
// document body or wrapped in an { document, idMap } envelope; both are handled.
// Older backends omit it entirely (idMap: null → caller falls back gracefully).
export async function saveWorkflow(document) {
  const res = await fetch('/workflow', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(document),
  });
  if (res.ok) {
    const body = await res.json();
    if (body && typeof body === 'object') {
      if (body.idMap && body.document) {
        return { ok: true, document: body.document, idMap: body.idMap };
      }
      if (body.idMap) {
        const { idMap, ...doc } = body;
        return { ok: true, document: doc, idMap };
      }
    }
    return { ok: true, document: body, idMap: null };
  }
  let body = null;
  try {
    body = await res.json();
  } catch {
    body = null;
  }
  return { ok: false, status: res.status, error: body?.error ?? null };
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

// Validate one editable text fragment against the lens. `kind` is
// `expression` | `assignment_target` | `identifier`. Returns `{ ok:true }` or
// `{ ok:false, error:{ code, message } }`. A missing endpoint (older backend)
// or any transport failure resolves to `{ ok:true, unsupported:true }` so the
// UI degrades to "no inline verdict" rather than showing false errors.
// `availableVars` is the scope the fragment is typed in: TypeScript rejects a
// fragment that reads a name it cannot see, so the field sends the names the
// node was projected with.
export async function validateFragment(kind, text, availableVars = []) {
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
export async function projectSource(source) {
  let res;
  try {
    res = await fetch('/project', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ source }),
    });
  } catch (err) {
    return { ok: false, error: { code: 'network', message: err?.message ?? String(err) } };
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

// Opens POST /run and yields parsed run-event payloads as they stream in.
// Each call is a brand-new run/invocation. `signal` aborts it (a new Play).
export async function* runWorkflow(signal) {
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

function parseFrame(frame) {
  const lines = frame.split('\n');
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
