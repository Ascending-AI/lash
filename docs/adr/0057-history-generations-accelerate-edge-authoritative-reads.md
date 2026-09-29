# 0057. History generations accelerate edge-authoritative reads

Date: 2026-08-09
Status: accepted

## Context

The shared-history model in ADR 0047 makes a session head a root into an
immutable parent-edge graph. SQL reads nevertheless rediscovered that ancestry
with recursive CTEs. A runtime commit also loaded and decoded the complete
active path merely to decide whether one requested ancestor was active. Those
queries made ordinary reads and commits scale with history depth even though
the graph is append-only and its ancestry cannot change.

Storing a second reachability authority would be worse than the recursion.
ADR 0024 removed a cached graph reference count because drift between the cache
and parent edges could make reclamation delete live history or retain dead
history. The same ruling applies to ancestry: edges and roots remain the only
authority.

## Decision

Every graph-node row stores two immutable facts assigned by the core commit
planner:

- `generation` is zero for a root and the checked increment of its parent's
  generation for every other node.
- `frame_node_id` is the node itself for `FrameOpen`, otherwise its parent's
  frame pointer.

Both SQL stores enforce `UNIQUE (session_id, generation)`. A session appends
only from its current head, so one producer session cannot contain two nodes at
one generation. The planner receives only `ParentNodeFacts` from the backend,
derives every appended node's facts, and preserves the existing cross-check
against the head's claimed current frame. Receipt replay never recomputes or
rewrites node facts.

A zero-copy fork writes `fork_lineage(session_id, ancestor_session_id,
fork_node_id, fork_generation)`. The core-owned `ForkPlan` is a total function
over the retained graph: each backend walks the selected node's parent edges to
generation zero, and core validates that root-to-node chain and takes the
greatest generation owned by each session. Heads, anchors, relation metadata,
and existing lineage rows select or retain a fork point but never reconstruct
its ancestry. Consequently a pinned node remains forkable after its owning
session is deleted even when no descendant session exists to carry lineage.
There is no carrier-row copy, carrier tie-break, or missing-owner fallback. A
session that owns no node is absent from lineage, including repeated rewinds
whose relation metadata names a superseded session. Copy-based child-session
creation writes no lineage rows.

The indexed read accelerator admits a node when either the session owns it or
one lineage row names its owning session and the node's generation is at or
below that ancestor row's ceiling. Ceilings stay per ancestor; combining them
into one global floor or ceiling can expose nodes appended to an older source
after a descendant forked.

Lineage is not authority. Every read that names a node — a history page
anchored at `Node(id)` or at a cursor, an admitted-base window, the
`contains_active_ancestor` predicate, and the commit path's requested-ancestor
fence — selects its candidate through the indexed ownership-or-ceiling
predicate and then admits it only if the requesting head leaf reaches it
through parent edges. The ceiling therefore narrows and never widens: a
deflated ceiling can deny a reachable node, and an inflated one can grant
nothing. Pages then walk down from an admitted anchor checking every parent
edge and generation, and window reads check every parent from the leaf down
to the frame base. A missing row, generation gap, or tombstone met along a
live head-to-node path is stored data corruption, following ADR 0024's ruling
that such a path cannot legitimately be partly reclaimed. SQLite performs the
selection and the confirmation inside one read transaction (or the commit's
write transaction for the fence); PostgreSQL uses `REPEATABLE READ`.

### Edge authority

Amended 2026-09-29 (FIG-4154). This section replaces the earlier argument that
the requested-ancestor fence was shielded by a complete-history state load;
ADR 0112 removed that load, and an inflated `B → A` ceiling then let B read,
predicate as active, and append past a node A appended after B forked.

The check is the head-path probe in
`lash-core-store/src/store_backend_support/head_path.rs`, which both backends
drive with the same two indexed reads: the head leaf's `(node_id, session_id,
generation)`, and, per owning session, that owner's lowest-generation row and
the row its parent edge names.

It rests on three facts about `graph_nodes`, none of which a lineage row can
change:

1. A node's parent, owning session and generation are written once in the
   same insert and never rewritten; a non-root node's generation is its
   parent's plus one.
2. `UNIQUE (session_id, generation)`: an owner holds at most one node per
   generation.
3. An owner appends only from its own head leaf (the planner refuses any other
   first parent), and a head leaf moves only by such an append or is set once
   when a fork creates the session. So an owner's first node has a foreign
   parent or none, and each later node's parent is the owner's previous node.

Hence an owner's nodes form one parent chain over contiguous generations, and
walking parent edges down from any node `e` of owner `s` visits every node of
`s` at or below `e`'s generation, then leaves `s` through the parent of `s`'s
lowest node and never returns. The probe starts at the head leaf and, while
undecided, jumps from the current owner's entry node to the parent of that
owner's lowest node. A candidate of owner `o` at generation `g` is reached iff
the walk enters `o` at a node of generation at least `g`: that candidate is
then `o`'s only node at `g`, which the chain passes through. Otherwise the walk
passes below `g` without entering `o`, or enters `o` below `g`. Generations
strictly decrease at every hop, so the probe ends after at most one hop per
owning session on the path — the same count as the session's lineage rows, and
independent of history depth. A row that contradicts the chain shape (an
owner's lowest node above the node the walk entered it at, a parent in the
same owner or not one generation lower, or a missing or retired parent under a
live path) is `StoredDataCorrupt`.

The fence runs the probe from the commit's own parent leaf inside the write
transaction, under the head compare-and-swap, so no preceding state load is
part of the argument. An anchor owned by the head leaf's owner costs one
statement beyond the candidate read and no hop.

Deriving frame facts also intentionally tightens `MissingFrameOpenAncestor`.
For a root append, the first appended node must be `FrameOpen`; a later
`FrameOpen` no longer rescues earlier root nodes. This replaces the former
"last FrameOpen among the appends" behavior and is an intended contract change:
every durable node must have a frame ancestor at the moment it is derived.

Reclamation is unchanged in authority and behavior. It derives liveness from
parent edges, heads, and anchors at every destructive step as required by ADR
0024 and ADR 0047. Lineage rows do not retain graph nodes, and deleting an
ancestor session does not invalidate descendant rows that name it. A lineage
row dies only with its own session.

The SQLite durable-core schema moves from 27 to 28 and the PostgreSQL component
schema from 39 to 40. Both stores reject older schemas and require recreation;
there is no backfill, migration, dual read, or compatibility path.

## Consequences

- Active-path materialization and append-ancestor checks no longer execute
  recursive SQL. The latter is one indexed candidate lookup plus the
  head-path probe, at most one indexed lookup per owning session, under
  commit authority.
- Deep fork reads carry one small lineage row per node-owning ancestor session,
  while zero-node sessions add no row.
- Fork creation is a rare, generation-bounded edge walk; correctness, including
  deleted-owner/no-carrier forks, owns this path.
- Corrupt accelerators can deny a reachable node, but they cannot grant access
  to a node that parent edges do not reach: every named anchor, the
  active-ancestor predicate and the requested-ancestor fence pass the
  head-path probe (§"Edge authority").
- Store conformance covers both directions of lineage/readability versus edge
  reachability, distinct ancestor ceilings, post-fork source appends,
  unrelated sessions, intermediate tombstones, deep fork chains, and
  generation/frame congruence. `inflated_fork_ceiling_cannot_expose_post_fork_source_nodes`
  pins the inflated-ceiling case on every anchor, the predicate and the
  fence, and the session-graph property law generates inflated ceilings
  against its reachability model.
