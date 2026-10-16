// A run as the canvas shows it. Everything here reads lash's reports: the
// site statuses of its execution overlay, the coverage `workflow::view::
// coverage` computed from its counts, and the path `workflow::view::path`
// mapped from its verified replay. Nothing is counted or inferred here.

const siteKey = (site) => JSON.stringify([site.unit, site.path ?? []]);

// Coverage by site key: how often each statement ran and each scope was
// entered. A statement or scope lash did not observe stays undefined.
export function counts(coverage, nodes) {
  return {
    nodes: new Map((nodes ?? []).map((node) => [siteKey(node.site), node])),
    scopes: new Map(
      (coverage?.scopes ?? []).map((s) => [siteKey(s.block), s.entries ?? undefined]),
    ),
  };
}

// The run as it stood at step `index` of its path: how often each statement
// and scope had started by then, and the statement starting at that step.
export function replayAt(steps, index) {
  const statements = new Map(),
    scopes = new Map();
  let active;
  for (const step of steps.slice(0, index + 1)) {
    const statement = step.kind === "statement";
    const site = siteKey(statement ? step.site : step.block);
    const held = statement ? statements : scopes;
    held.set(site, (held.get(site) ?? 0) + 1);
    if (statement) active = site;
  }
  return { statements, scopes, active };
}

// Canonical node coverage and exact action state. Statement replay never
// supplies action issues; an absent observation remains unknown.
export function nodeState(node, run) {
  const action = node.node_kind === "action";
  const site = siteKey(node.site);
  const total = run.counts.nodes.get(site)?.runs ?? undefined;
  // Statement replay has no action-issue trace. Leave action evidence unknown
  // while replaying, rather than manufacturing action counts from starts.
  if (run.replay) {
    if (action) return {};
    const runs = run.replay.statements.get(site) ?? 0;
    return { runs, state: run.replay.active === site ? "active" : runs ? "done" : total === 0 ? "skipped" : undefined };
  }
  const status = action ? run.statuses.get(site) : undefined;
  const error = action ? run.errors.get(site) : undefined;
  const state = { failed: "failed", started: "active", waiting: "waiting",
    incomplete: "incomplete", succeeded: "done" }[status]
    ?? (total > 0 ? "done" : run.ended && total === 0 ? "skipped" : undefined);
  return { state, runs: total, error };
}

// Which arms of the `if` at `site` the run took, by scope kind (`then`,
// `else`): true when lash counted entering it, false when it counted none.
export function armsTaken(view, run, site) {
  const arms = {};
  for (const scope of view.scopes) {
    if (!["then", "else"].includes(scope.kind)) continue;
    if (!scope.statement || siteKey(scope.statement) !== siteKey(site)) continue;
    const block = siteKey(scope.block);
    const entries = run.replay
      ? (run.replay.scopes.get(block) ?? 0)
      : run.counts.scopes.get(block);
    if (entries !== undefined) arms[scope.kind] = entries > 0;
  }
  return arms;
}

// A value as a node's result chip: compact JSON cut to `limit` characters.
export function chip(value, limit = 34) {
  const text = typeof value === "string" ? value : JSON.stringify(value);
  if (text === undefined) return undefined;
  return text.length > limit ? `${text.slice(0, limit - 1)}…` : text;
}
