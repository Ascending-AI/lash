# Issue tracker: Linear

Issues, tickets, and decision maps for this repo live in **Linear** (team **`figments`**, project **`lash`**). Use the **Linear MCP tools** (`mcp__linear__*`) for all operations.

Issues are keyed `FIG-<n>`. Refer to issues by their **title/name**, never by a bare key; the key rides inside the name as a link.

## Conventions

- **Create an issue**: `mcp__linear__save_issue` with `team: "figments"`, `project: "lash"` (+ `title`, `description` as Markdown, `parentId`, `labels`, `blockedBy`, `priority` as needed). No `id` = create.
- **Read an issue**: `mcp__linear__get_issue` (`id: "FIG-<n>"`, `includeRelations: true` for blockers). For comments: `mcp__linear__list_comments`.
- **List issues**: `mcp__linear__list_issues` (filter by `team`, `project`, `state`, `label`, `parentId`, `assignee`, `query`).
- **Comment**: `mcp__linear__save_comment` (`issueId`, `body`).
- **Apply / change labels**: `mcp__linear__save_issue` with `id` + `labels` (replaces the full label set; include existing labels you want to keep). Create new label strings first with `mcp__linear__create_issue_label` if they don't exist.
- **Set state / close**: `mcp__linear__save_issue` with `id` + `state` (`"Done"`, `"Canceled"`, etc.; Linear workflow states, not GitHub open/closed).
- **Assign**: `mcp__linear__save_issue` with `id` + `assignee` (user id, name, email, or `"me"`).
- **Relations**: `blockedBy` / `blocks` on `save_issue` are append-only; `removeBlockedBy` / `removeBlocks` to clear.

Codex agents run with MCPs disabled (see the global agent rules), so a spec handed to one must **inline** any Linear context it needs; it cannot fetch a ticket itself.

Do not file housekeeping/teardown/process chores here; Linear is code-facing only.

## Pull requests as a request surface

**PRs as a request surface: no.** Code review happens on GitHub PRs; Linear holds the work items. `/triage` does not scan PRs.

## When a skill says "publish to the issue tracker"

Create a Linear issue in team `figments`, project `lash`.

## When a skill says "fetch the relevant ticket"

`mcp__linear__get_issue { id: "FIG-<n>", includeRelations: true }`.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single Linear issue; its **tickets** are Linear sub-issues (children).

- **Map**: one Linear issue labelled **`wayfinder:map`**, holding the Destination / Notes / Decisions-so-far / Not-yet-specified / Out-of-scope body (the `/wayfinder` skill's map template). Create with `save_issue { team: "figments", project: "lash", title: <map name>, labels: ["wayfinder:map"] }`.
- **Child ticket**: a Linear issue with `parentId: "<map key>"`, labelled by type: **`wayfinder:research` | `wayfinder:prototype` | `wayfinder:grilling` | `wayfinder:task`**. Every wayfinder issue (map + tickets) carries a `wayfinder:*` label so the whole map is filterable. Once claimed, assign it to the driving dev.
- **Blocking**: Linear native relations, e.g. `save_issue { id: <child>, blockedBy: ["<blocker key>"] }`. A ticket is unblocked when every blocker is in a terminal state (`Done`/`Canceled`). Read blockers via `get_issue { includeRelations: true }`.
- **Frontier query**: `list_issues { parentId: "<map key>", state: <non-terminal> }` → drop any child with an open blocker or an assignee; first in map order wins.
- **Claim**: `save_issue { id: <n>, assignee: "me" }` (the session's first write).
- **Resolve**: `save_comment { issueId: <n>, body: "<the decision>" }`, then `save_issue { id: <n>, state: "Done" }`, then append a one-line gist + link to the map's Decisions-so-far.

Labels used: `wayfinder:map`, `wayfinder:research`, `wayfinder:prototype`, `wayfinder:grilling`, `wayfinder:task` (create in the `figments` team if absent).
