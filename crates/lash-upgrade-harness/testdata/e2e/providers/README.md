These are authored, scrubbed HTTP transcripts for H1's deterministic provider
fixtures. They are not captures from an external model service. The fixture
serves the literal chunk bytes, headers and connection termination recorded
here to the production OpenAI-compatible HTTP client.

The request bodies pin the H1 session specification (`e2e/model`, `H1
transcript.`, Standard's built-in `batch` tool). They contain no wildcard,
request-body normalization, authorization header, cookie or captured user data.
Changing a model request requires reviewing the corresponding literal body.
The occurrence identity records ordering even when a retry repeats identical
request bytes. Every expected occurrence must match and finish; extras,
misordering, missing occurrences and zero matches fail reconciliation.

- `s03-tools.json`: a real tool body posts to the independent keyed ledger.
  The recovery controller holds the reply after synced acceptance, kills the
  owned host, and checks cold redelivery without a second mutation.
- `s04-tools.json`: a state-command tool proposal transcript. Its proposal/ACK
  cut choreography is deferred under the arc scope hold.
- `s06-tools.json`: a Standard batch invokes two reported-retry bodies. The
  recovery controller uses actual durable retry schedules and a pending V7
  Sleep, then releases the second attempts in reverse readiness order.
- `s07-tools.json`: the initial batch proposal only. Cancellation during the
  pending backoff must settle without another provider request or tool attempt.
- `s26-rate-limit.json`: HTTP 429 with `retry-after`, then two SSE events
  carrying one answer and usage, followed by `[DONE]`. `answer-started` holds
  after the first event is written so the observer can disconnect and reattach.
- `s26-partial-disconnect.json`: visible partial output, then a socket close
  without the terminating HTTP chunk. The production client's non-retryable
  transport policy must terminate the Run without committing that partial
  answer or issuing another paid generation.
- `s27-auth-next-run.json`: HTTP 401, then a fresh user input in the same
  session, a successful answer and usage. The fresh request includes the
  earlier committed user message; it is a separate Run, not a retry of 401.

HTTP write/acceptance events describe the outside transport only. Journal
proposal and durable ACK cuts, tool attempt identities, Run terminals and
namespace publication must be collected independently from actual Restate
journals and store reads. The synced keyed effect ledger survives host kills;
its JSONL deliveries and mutation count never substitute for those journals.

These lane fixtures are authored development inputs. Z04 owns recapturing
journal and release fixture shapes after the tool cutovers. Product-host S26/S27
acceptance is deferred; the cheap production-client witnesses are separate.
