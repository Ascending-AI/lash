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

Lineage is not authority. A cross-session `load_node` that passes the indexed
predicate fetches the involved sessions' bounded generation range in one query,
then confirms the result by walking parent edges in memory from the requesting
session's live head. A mismatched node is not readable. A missing row,
generation gap, or tombstone encountered within that bounded segment is stored
data corruption. Returning corruption, rather than treating a mid-segment
tombstone as an ordinary unreadable candidate, follows ADR 0024's ruling: a
live head-to-candidate path cannot legitimately be partly reclaimed while
edges and roots are authority. Session-graph reads likewise validate a
continuous generation/parent chain and every frame pointer after selecting
rows through the accelerator. SQLite performs candidate selection and this
confirmation inside one read transaction; PostgreSQL uses `REPEATABLE READ`.

The commit-path requested-ancestor fence may use lineage to select one indexed
candidate because the normal runtime can construct that commit only from
`load_persisted_session_state`. A false lineage row makes that preceding session
load fail `StoredDataCorrupt` when its rows do not form the head's continuous
edge path, before a commit can be built. Thus the fence is shielded by the
edge-authoritative state-load contract; this is a required argument, not an
incidental property of current call order.

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
  recursive SQL. The latter is one indexed node lookup under commit authority.
- Deep fork reads carry one small lineage row per node-owning ancestor session,
  while zero-node sessions add no row.
- Fork creation is a rare, generation-bounded edge walk; correctness, including
  deleted-owner/no-carrier forks, owns this path.
- Corrupt accelerators can deny a reachable node, but they cannot grant access
  to a node that parent edges do not reach because `load_node` confirms the
  edge path.
- Store conformance covers both directions of lineage/readability versus edge
  reachability, distinct ancestor ceilings, post-fork source appends,
  unrelated sessions, intermediate tombstones, deep fork chains, and
  generation/frame congruence.
