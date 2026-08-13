# Claim nonces scope session-lease lifecycle and execution fences, not commit authority

## Status

Accepted.

## Context

A session-execution lease carries three identities with different jobs: stable
holder identity, a token persisted on the current claim, and a monotonic fencing
generation. Before FIG-904, same-incarnation re-claims reused both durable
tokens. A completion retained by an earlier holder was therefore
indistinguishable from the current claim at release time and could clear the
successor's live lease. Removing out-of-band release avoided that corruption but
made every cleanly dropped guard wait for TTL before a peer could reclaim it.

The session head already has independent write-time arbitration. A commit checks
the holder and fencing generation, then the session-head compare-and-set
linearizes competing writers. The open decision was whether rotating the claim
token should invalidate only renewal and release, or should additionally become
a second commit fence that displaces an overlapping holder in the same
incarnation.

The FIG-904 systems survey found three recurring roles: stable holder identity,
claim-instance identity for lock lifecycle, and a monotonic fence checked by the
resource at write time. That comparison originally led Lash to keep the claim
token out of execution-fence authority. The FIG-1064 ticket premise was
inaccurate: PostgreSQL, SQLite, in-memory, and the perf store all accepted a
retained same-generation guard on the execution fence after token rotation.
PostgreSQL checked token equality only for renewal and release, just like the
other backends.

## Decision

The lease token is a per-claim nonce and one required component of
execution-fence authority as well as lock lifecycle authority. It does not
become session-head commit authority.

- Every distinct claim, including exact owner/incarnation/executor reentry, mints a fresh
  `LeaseClaimNonce`. Retrying one ambiguous claim reuses the same nonce and must
  return the exact durable token without advancing generation or changing
  `claimed_at`. The nonce has no value-taking constructor and no `Display`
  implementation; hosts can clone a minted value for retry but cannot derive one
  from stable owner data.
- Same-executor reentry therefore rotates the lock-lifecycle token, and releasing
  that reentered claim clears the durable lane row. A second runtime open under
  the same host owner has a different executor id and is Busy, never reentrant.
  A nested runtime commit whose caller already
  holds the session-execution lane borrows the outer guard's current authority:
  it makes no fresh claim, performs no rotation, and performs no release on
  success or failure. The outer guard remains the owner of renewal and release.
  The former nested-release signal and Agent Frame handoff transfer are removed.
- Renewal never rotates the nonce. Renewal and standalone release require exact
  `(owner, lease token)` equality and return named refusals when the claim is no
  longer current. Backend implementations must serialize renewal against claim
  rotation; a conditional write that affects no row is a refusal, never success.
- Same-incarnation rotation preserves the fencing generation and the original
  `claimed_at`. It changes which claim may renew or release, not when the holder
  first acquired the lane.
- Fenced execution operations require one core-owned predicate: the presented
  authority is bound to the requested session, the durable row exists, the
  holder incarnation and fencing generation match, the lease is unexpired, and
  the current lease token matches. A retained guard from before same-owner token
  rotation is rejected at its next fenced claim. Session-head commit authority
  remains the independent head CAS.
- Borrowed commits use that exact execution predicate inside the store's commit
  transaction before receipt replay or mutation. There is no commit-specific
  exemption for stale or lapsed guards. The fresh-lease commit path retains
  rotation and atomic release for lane-less callers.
- A stale ancillary release carried by a valid commit never vetoes that commit
  and never clears the successor. A named in-band release refusal after commit
  is terminal and benign rather than `StoreCommitFailed`.
- Any operation that must forcibly displace an overlapping holder advances the
  monotonic generation. It must not overload the unordered claim nonce as a
  write fence.

The claim and renewal paths use the same per-session serialization discipline on
each backend: one in-memory transaction lock, one SQLite writer transaction, and
one PostgreSQL advisory lock plus row lock. Release remains one atomic
owner-and-token-predicated write. FIG-1064's SQLite/PostgreSQL predicate
divergence is resolved by routing PostgreSQL, SQLite, in-memory, and perf-store
execution checks through the core predicate.

This predicate and nesting contract are the binding 2026-08-08 FIG-1063 ruling:
borrow when the caller holds the lane; acquire fresh and rotate only when it
does not. A borrowed nested write that advances the durable head explicitly
marks the runtime's resident graph stale so the next physical turn reloads
before planning; guard continuity alone cannot prove graph freshness. FIG-1072
owns the renewal-predicate follow-up.

A rolling deployment on any backend must not let two binaries with different
token-rotation semantics share one incarnation identity. Incarnation identifies
one compatible execution authority; a host must mint a new incarnation when
replacing such an authority across a protocol change.

## Consequences

- A retained completion names only its own claim. Drop may again attempt
  best-effort release immediately; a late attempt is refused without touching a
  successor, while TTL remains the fallback for backend failure.
- A rotated or reclaimed lease cannot claim later runtime work, even when its
  holder incarnation and fencing generation still match.
- Same-owner `try_claim` always rotates the token and returns an acquisition,
  never `Busy`. Incarnation-reusing hosts therefore fence overlapping attempts
  at claim time; for example, Restate retries that reuse
  `restate_process_execution` identity rotate each other's execution fence.
  Default runtime owners use a per-runtime UUID and are unaffected.
- Fresh-acquire and takeover retries cannot double-bump generation, and the
  public nonce type prevents stable host identity from silently defeating
  per-claim uniqueness.
- Execution-fence authority is decided once in core from incarnation,
  generation, expiry, and current-token equality. Commit authority remains the
  ADR 0029 head CAS. No durable schema, existing ID, or serialized authority
  shape changes.
- Nested commits no longer invalidate the retained guard or emit a handoff
  transfer record. Their authority is observable through the borrowed commit
  fence, while durable-head freshness is handled independently by an explicit
  reload marker.
