// A run as the canvas shows it. Everything here reads lash's reports: the
// site statuses of its execution overlay, the coverage `workflow::view::
// coverage` computed from its counts, and the path `workflow::view::path`
// mapped from its verified replay. Nothing is counted or inferred here.

const siteKey = (site) => JSON.stringify([site.unit, site.path ?? []]);

// Coverage by site key: how often each statement ran and each scope was
// entered. A statement or scope lash did not observe stays undefined.
export function counts(coverage) {
  return {
    statements: new Map(
      (coverage?.statements ?? []).map((s) => [siteKey(s.site), s.runs ?? undefined]),
    ),
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

// What `statement`'s node shows of `run`: its state (active, waiting,
// failed, incomplete, done or skipped; undefined while it has not run, or
// when nothing observed it) and how often it ran, read from the statuses
// of every action it issues. `run` holds the site statuses and errors by
// action site, the counts, whether the run ended,
// and, while the path is being replayed, the replay step.
export function nodeState(statement, run) {
  const site = siteKey(statement.site);
  const total = run.counts.statements.get(site);
  if (run.replay) {
    const runs = run.replay.statements.get(site) ?? 0;
    const state =
      run.replay.active === site
        ? "active"
        : runs
          ? "done"
          : total === 0
            ? "skipped"
            : undefined;
    return { state, runs };
  }
  // A statement's state is its most telling action's: a failure, then a
  // wait, then one under way.
  const actions = (statement.actions ?? []).map(siteKey);
  const statuses = actions.map((action) => run.statuses.get(action));
  const status = ["failed", "waiting", "started", "incomplete", "succeeded"].find(
    (candidate) => statuses.includes(candidate),
  );
  const error = actions
    .map((action) => run.errors.get(action))
    .find((error) => error !== undefined);
  const state =
    status === "failed"
      ? "failed"
      : status === "started"
        ? "active"
        : status === "waiting"
          ? "waiting"
          : status === "incomplete"
            ? "incomplete"
            : status === "succeeded" || total > 0
              ? "done"
              : run.ended && total === 0
                ? "skipped"
                : undefined;
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
