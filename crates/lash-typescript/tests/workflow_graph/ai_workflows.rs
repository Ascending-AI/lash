//! Workflows written the way a model writes them (FIG-5578).
//!
//! A host that edits workflows mostly receives generated ones, and a
//! generator reaches for whatever the language has: handlers that catch and
//! rethrow, cleanup in `finally`, closures over loop state, inline process
//! definitions inside other definitions, computed member writes, early exits
//! from nested loops. Each program here leans on several of those at once.
//! The corpus laws run over them like over every other corpus, and the
//! document differential runs each one published from its workflow document
//! against the one admitted from this source.

#![allow(
    dead_code,
    reason = "each including binary reads the constants it pins"
)]

/// Fan out lookups, retry the ones that fail, and report through `finally`.
const RETRYING_FAN_OUT: &str = r#"const queries = ["alpha", "beta", "gamma"];
const results = {};
let attempts = 0;
for (const query of queries) {
  let answer = null;
  let tries = 0;
  while (answer === null && tries < 2) {
    tries += 1;
    attempts += 1;
    try {
      if (query === "beta" && tries === 1) {
        throw { code: "RATE_LIMITED", query };
      }
      answer = await tools.lookup({ query });
    } catch (error) {
      await tools.echo({ value: { retrying: query, reason: error.code } });
    } finally {
      await tools.echo({ value: `attempt ${tries} for ${query}` });
    }
  }
  results[query] = answer;
}
finish({ attempts, results });
"#;

/// A supervisor that starts a worker per item; each worker defines and
/// starts its own child and hands back a closure-built summary.
const NESTED_SUPERVISOR: &str = r#"const worker = async (item: unknown) => {
  const audit = async (stage: unknown) => {
    await tools.echo({ value: { audit: stage } });
    return stage;
  };
  const started = await processes.start({ definition: audit, args: { stage: "start" } });
  const fetched = await web.fetch({ url: item.url });
  const describe = (label: string) => `${label}:${item.id}`;
  if (fetched === null) {
    throw new Error(describe("missing"));
  }
  return { id: item.id, label: describe("done"), started };
};
const items = [{ id: 1, url: "https://example.test/1" }, { id: 2, url: "https://example.test/2" }];
const handles = [];
for (const item of items) {
  const handle = await processes.start({ definition: worker, args: { item } });
  handles.push(handle);
}
finish(handles.length);
"#;

/// Aggregation with closures that capture and update loop state, computed
/// member writes and a pipeline of array callbacks.
const CLOSURE_AGGREGATION: &str = r#"const rows = [
  { team: "red", score: 3 },
  { team: "blue", score: 5 },
  { team: "red", score: 4 },
];
const totals = {};
const bump = (team: string, by: number) => {
  totals[team] = (totals[team] ?? 0) + by;
  return totals[team];
};
const seen = [];
for (const row of rows) {
  const total = bump(row.team, row.score);
  seen.push(`${row.team}=${total}`);
}
const ranked = Object.keys(totals)
  .map((team) => ({ team, total: totals[team] }))
  .filter((entry) => entry.total > 4)
  .sort((left, right) => right.total - left.total);
const report = await tools.echo({ value: { ranked, seen } });
finish(report);
"#;

/// An approval gate: a guarded early exit, a nested handler that rethrows,
/// and cleanup that runs on every path.
const GUARDED_APPROVAL: &str = r#"const request = { id: "req-7", amount: 1200, tags: ["urgent"] };
let status = "pending";
let cleaned = 0;
try {
  try {
    if (request.amount > 1000) {
      const decision = await host.approval({ request });
      if (decision === null) {
        throw new Error("no decision");
      }
      status = "approved";
    } else {
      status = "auto";
    }
  } catch (error) {
    status = "escalated";
    throw error;
  } finally {
    cleaned += 1;
    await tools.echo({ value: { cleaned, status } });
  }
} catch (outer) {
  await tools.echo({ value: { failed: String(outer) } });
} finally {
  cleaned += 1;
}
finish({ status, cleaned });
"#;

/// A grid search that leaves nested loops early and skips cells, with a
/// counter written through a computed path.
const EARLY_EXIT_SEARCH: &str = r#"const grid = [[1, 2, 3], [4, 5, 6], [7, 8, 9]];
const visited = { cells: [], rows: 0 };
let found = null;
for (const row of grid) {
  visited.rows += 1;
  for (const cell of row) {
    if (cell % 2 === 0) {
      continue;
    }
    visited.cells[visited.cells.length] = cell;
    if (cell > 4) {
      found = await tools.echo({ value: cell });
      break;
    }
  }
  if (found !== null) {
    break;
  }
}
finish({ found, visited });
"#;

/// A pipeline of stages held as functions in a record, called through the
/// record so each stage reads its neighbours, with a failure that stops it.
const STAGED_PIPELINE: &str = r#"const pipeline = {
  log: [],
  clean(text: string) {
    this.log.push("clean");
    return text.trim().toLowerCase();
  },
  split(text: string) {
    this.log.push("split");
    return this.clean(text).split(" ");
  },
};
const words = pipeline.split("  Ship The Thing  ");
let longest = "";
for (const [index, word] of words.entries()) {
  if (word.length > longest.length) {
    longest = word;
  }
  await tools.echo({ value: { index, word } });
}
let outcome = "ok";
try {
  await tools.err({ value: longest });
} catch (error) {
  outcome = "failed";
}
finish({ longest, outcome, log: pipeline.log });
"#;

/// A durable definition that waits, branches on what it waited for and
/// starts a follow-up definition from inside a handler.
const WAITING_DEFINITION: &str = r#"const follow_up = async (reason: unknown) => {
  await sleep(5);
  return { reason };
};
const reviewer = async (ticket: unknown) => {
  let verdict = "open";
  try {
    const decision = await host.approval({ request: ticket });
    await sleep(10);
    verdict = decision === null ? "timed-out" : "decided";
    if (verdict === "timed-out") {
      throw new Error("review timed out");
    }
  } catch (error) {
    const chained = await processes.start({ definition: follow_up, args: { reason: verdict } });
    return { verdict, chained };
  }
  return { verdict };
};
const handle = await processes.start({ definition: reviewer, args: { ticket: { id: 42 } } });
const again = await processes.start({ definition: reviewer, args: { ticket: null } });
finish([handle, again]);
"#;

/// Every AI-style workflow, by name.
pub(crate) const ALL: &[(&str, &str)] = &[
    ("retrying-fan-out", RETRYING_FAN_OUT),
    ("nested-supervisor", NESTED_SUPERVISOR),
    ("closure-aggregation", CLOSURE_AGGREGATION),
    ("guarded-approval", GUARDED_APPROVAL),
    ("early-exit-search", EARLY_EXIT_SEARCH),
    ("staged-pipeline", STAGED_PIPELINE),
    ("waiting-definition", WAITING_DEFINITION),
];
