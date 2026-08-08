# Claim nonces scope session-lease lifecycle, not commit authority

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
resource at write time. Chubby, etcd, ZooKeeper, and HDFS QJM keep lifecycle
identity separate from resource-side fencing. The DynamoDB lock client is the
closest lifecycle mechanism: owner plus a random record-version nonce predicates
heartbeat and release. Temporal layers task identity on top of write arbitration
only because its row and shard fences are blind to a stale worker completing a
superseded task. Lash's head CAS is not blind to same-generation overlap; it is
the operation that already decides which overlapping commit publishes. Kafka
and QJM displace writers with monotonic epochs, not random nonces.

## Decision

Adopt position 1: the lease token is a per-claim nonce scoped exclusively to the
lock lifecycle.

- Every distinct claim, including same-incarnation reentry, mints a fresh
  `LeaseClaimNonce`. Retrying one ambiguous claim reuses the same nonce and must
  return the exact durable token without advancing generation or changing
  `claimed_at`. The nonce has no value-taking constructor and no `Display`
  implementation; hosts can clone a minted value for retry but cannot derive one
  from stable owner data.
- Same-owner reentry therefore rotates the lock-lifecycle token, and releasing
  that reentered claim clears the durable lane row. A logical Agent Frame chain
  that retained the earlier guard uses the frame-handoff transfer boundary to
  reacquire only when a nested commit proves that rotation occurred. A locally
  expired or lost guard is never silently reacquired there. FIG-1063 owns the
  complete nesting contract beyond this boundary mechanism.
- Renewal never rotates the nonce. Renewal and standalone release require exact
  `(owner, lease token)` equality and return named refusals when the claim is no
  longer current. Backend implementations must serialize renewal against claim
  rotation; a conditional write that affects no row is a refusal, never success.
- Same-incarnation rotation preserves the fencing generation and the original
  `claimed_at`. It changes which claim may renew or release, not when the holder
  first acquired the lane.
- Execution writes deliberately ignore the nonce. Holder plus live fencing
  generation and the session-head CAS remain the sole commit authority. Two
  same-generation holders may overlap; either may win a current-head CAS and the
  other loses through the ordinary head conflict.
- A stale ancillary release carried by a valid commit never vetoes that commit
  and never clears the successor. A named in-band release refusal after commit
  is terminal and benign rather than `StoreCommitFailed`.
- Any operation that must forcibly displace an overlapping holder advances the
  monotonic generation. It must not overload the unordered claim nonce as a
  write fence.

The claim and renewal paths use the same per-session serialization discipline on
each backend: one in-memory transaction lock, one SQLite writer transaction, and
one PostgreSQL advisory lock plus row lock. Release remains one atomic
owner-and-token-predicated write. FIG-1064 tracks the SQLite/PostgreSQL predicate
divergence in those enforcement paths.

A rolling deployment must not let two binaries with different token-rotation
semantics share one incarnation identity. Incarnation identifies one compatible
execution authority; a host must mint a new incarnation when replacing such an
authority across a protocol change.

## Consequences

- A retained completion names only its own claim. Drop may again attempt
  best-effort release immediately; a late attempt is refused without touching a
  successor, while TTL remains the fallback for backend failure.
- A rotated or reclaimed lease can race a completed turn's cleanup without
  turning a committed turn into a false store-commit failure.
- Fresh-acquire and takeover retries cannot double-bump generation, and the
  public nonce type prevents stable host identity from silently defeating
  per-claim uniqueness.
- Commit authority remains single-homed in the existing generation and head-CAS
  model established by ADR 0029. No durable schema or serialized authority shape
  changes: the existing opaque token column stores different values under a
  stricter lifecycle contract.
