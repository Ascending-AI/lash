# Issue tracker: Linear

Issues and tickets for this repo live in **Linear**, in the **`lash` project** on team **`figments`**. Use the **Linear MCP tools** (`mcp__linear__*`) for all operations.

Issues are keyed `FIG-<n>`. Refer to issues by their **title/name**, never by a bare key; the key rides inside the name as a link.

This repo has no offline Linear helper script. An agent running without the Linear MCP (for example a sandboxed `codex exec` run) cannot read the tracker, so the ticket context it needs must be inlined into its prompt.

## Conventions

- **Create an issue**: `mcp__linear__save_issue` with `team: "figments"`, `project: "lash"` (+ `title`, `description` as Markdown, `parentId`, `labels`, `blockedBy`, `priority` as needed). No `id` = create.
- **Read an issue**: `mcp__linear__get_issue` (`id: "FIG-<n>"`, `includeRelations: true` for blockers). For comments: `mcp__linear__list_comments`.
- **List issues**: `mcp__linear__list_issues` (filter by `team`, `project`, `state`, `label`, `parentId`, `assignee`, `query`).
- **Comment**: `mcp__linear__save_comment` (`issueId`, `body`).
- **Apply / change labels**: `mcp__linear__save_issue` with `id` + `labels` (replaces the full label set; include existing labels you want to keep). Create new label strings first with `mcp__linear__create_issue_label` if they don't exist.
- **Set state / close**: `mcp__linear__save_issue` with `id` + `state` (`"Done"`, `"Canceled"`, etc.; Linear workflow states, not GitHub open/closed).
- **Assign**: `mcp__linear__save_issue` with `id` + `assignee` (user id, name, email, or `"me"`).
- **Relations**: `blockedBy` / `blocks` on `save_issue` are append-only; `removeBlockedBy` / `removeBlocks` to clear.

Do not file housekeeping/teardown/process chores here; Linear is code-facing only.

## Pull requests as a request surface

**PRs as a request surface: no.** Code review happens on GitHub PRs; Linear holds the work items. Triage does not scan PRs.

## When a skill says "publish to the issue tracker"

Create a Linear issue in team `figments`, project `lash`.

## When a skill says "fetch the relevant ticket"

`mcp__linear__get_issue { id: "FIG-<n>", includeRelations: true }`.

## Parent and child operations

A multi-ticket effort is one **parent** Linear issue with its slices and decisions as **sub-issues** ([way-of-working.md](way-of-working.md), [ticket-style.md](ticket-style.md)).

- **Parent**: `save_issue { team: "figments", project: "lash", title: <effort name> }`, body carrying the destination plus the one-line-per-child index.
- **Child ticket**: a Linear issue with `parentId: "<parent key>"`. Once claimed, assign it to the driving dev.
- **Blocking**: Linear native relations, e.g. `save_issue { id: <child>, blockedBy: ["<blocker key>"] }`. A ticket is unblocked when every blocker is in a terminal state (`Done`/`Canceled`). Read blockers via `get_issue { includeRelations: true }`.
- **Frontier query**: `list_issues { parentId: "<parent key>", state: <non-terminal> }` → drop any child with an open blocker or an assignee; first in index order wins.
- **Claim**: `save_issue { id: <n>, assignee: "me" }` (the session's first write).
- **Resolve a decision**: `save_comment { issueId: <n>, body: "<the decision>" }`, then echo the conclusion into the ticket body, then `save_issue { id: <n>, state: "Done" }`, then append a one-line gist plus link to the parent's index.
