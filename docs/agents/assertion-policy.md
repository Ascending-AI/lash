# Runtime assertion policy

Release builds enforce internal postconditions with `assert!` and `assert_eq!`.
A newly committed store receipt must advance the commit's expected head revision; adopting it
must advance resident state. A lease attached by either the fresh or borrowed
path must belong to the commit's session. These checks detect broken internal
contracts and panic immediately; they are not debug-only diagnostics.

Receipt replay makes no new commit and may return a previously committed revision.
It is exempt from the store-boundary advance check; append replay refreshes
resident state rather than adopting the old receipt as a new commit.
Receipt replay, flagged by the store via `receipt_replayed`, is the only
non-advancing receipt the boundary accepts: every store — test doubles included
— must flag replayed receipts and advance the head revision on fresh commits
(a reopened session's next turn is a fresh commit), and a double that fails
either half is fixed to conform rather than the assertion being widened.

Expected contention, stale claims, and invalid external input return existing
typed errors. The execution lease remains advisory (ADR 0029): session identity
is a consistency assertion, not a second commit authority. A busy holder still
allows the head-CAS attempt, and expiry or ownership is not asserted here.

Recovered settlement permits at most one retry per originally claimed queue
batch or turn-input row, plus the initial attempt. Each retry removes a
superseded recovered row. Exhaustion returns the last typed store error; it
never widens a timeout or retries an unchanged settlement indefinitely.

Claim-settlement symmetry and usage conservation retain their existing checks.
Tests pin failures at the receipt and adoption boundaries and the settlement
attempt bound. Assertions are identical in development and optimized builds.
