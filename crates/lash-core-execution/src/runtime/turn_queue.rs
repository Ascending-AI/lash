#[allow(unused_imports)]
use crate::SessionId;
pub use lash_core_store::queued_work_vocabulary::*;

/// An accepted session command waiting for its queue completion to be
/// committed atomically with the new session head.
#[derive(Clone, Debug)]
pub struct SessionCommandSettlementHandle {
    pub receipt: SessionCommandReceipt,
}

/// Host policy bounding one automatically selected queued-work admission.
///
/// Row count and maximum pending age retain Lash defaults because a poor
/// choice affects batching efficiency rather than context-window correctness.
/// The action reserve is required and has no Lash default; since FIG-1313 it is
/// advisory evidence for a custom [`QueuedDrainPolicy`](crate::QueuedDrainPolicy)
/// plus a misconfiguration check, because no shipped drain mode does token
/// arithmetic (see [`action_token_reserve`](Self::action_token_reserve)).
///
/// How much of the legal, FIFO-ordered admissible prefix actually drains on one
/// wake is a separate, explicitly named host policy: the
/// [`QueuedDrainPolicy`](crate::QueuedDrainPolicy) selected by
/// [`with_drain_mode`](Self::with_drain_mode) or
/// [`with_drain_policy`](Self::with_drain_policy), defaulting to
/// [`DrainMode::OneAtATime`](crate::DrainMode::OneAtATime).
#[derive(Clone, Debug)]
pub struct QueuedWorkBatchingConfig {
    action_token_reserve: std::num::NonZeroUsize,
    max_rows: std::num::NonZeroUsize,
    max_pending_age: std::time::Duration,
    max_turn_input_admission: std::num::NonZeroUsize,
    /// `None` selects the documented Lash default,
    /// [`DrainMode::OneAtATime`](crate::DrainMode::OneAtATime), so the
    /// configuration stays `const`-constructible.
    drain_policy: Option<std::sync::Arc<dyn crate::QueuedDrainPolicy>>,
}

impl PartialEq for QueuedWorkBatchingConfig {
    /// Resolving first keeps the shipped modes honest: an unset policy and an
    /// explicit [`with_drain_mode`](Self::with_drain_mode) naming the same mode
    /// share one instance and compare equal. Two separately constructed *custom*
    /// policies never do, even when they behave identically — Lash cannot prove
    /// that, so it does not claim it. Hosts that compare configurations holding
    /// custom policies should share one `Arc`.
    fn eq(&self, other: &Self) -> bool {
        self.action_token_reserve == other.action_token_reserve
            && self.max_rows == other.max_rows
            && self.max_pending_age == other.max_pending_age
            && self.max_turn_input_admission == other.max_turn_input_admission
            && std::sync::Arc::ptr_eq(&self.drain_policy(), &other.drain_policy())
    }
}

impl QueuedWorkBatchingConfig {
    /// Default upper bound on rows coalesced into one fresh turn admission.
    ///
    /// Hosts may replace this efficiency bound with [`Self::with_max_rows`].
    /// Interrupted-admission redrive preserves the predecessor composition even
    /// when it contains more rows than this fresh-admission default.
    pub const DEFAULT_MAX_ROWS: usize = 64;
    /// Default age at which the oldest compatible row is admitted alone instead
    /// of being coalesced with later ready rows.
    ///
    /// Hosts may replace this latency bound with
    /// [`Self::with_max_pending_age`].
    pub const DEFAULT_MAX_PENDING_AGE: std::time::Duration = std::time::Duration::from_secs(30);
    /// Default upper bound on pending next-turn inputs one idle admission
    /// offers the drain policy.
    ///
    /// Hosts may replace it with [`Self::with_max_turn_input_admission`].
    pub const DEFAULT_MAX_TURN_INPUT_CLAIM: usize = 64;

    /// These bounds apply to fresh admissions. Redriving an interrupted admission keeps
    /// its already-journaled composition intact.
    ///
    /// # Panics
    ///
    /// Panics when `action_token_reserve` is zero.
    #[expect(
        clippy::expect_used,
        reason = "the default queued-work row bound is a non-zero literal"
    )]
    pub const fn new(action_token_reserve: usize) -> Self {
        let Some(action_token_reserve) = std::num::NonZeroUsize::new(action_token_reserve) else {
            panic!("queued-work action token reserve must be non-zero");
        };
        Self {
            action_token_reserve,
            max_rows: std::num::NonZeroUsize::new(Self::DEFAULT_MAX_ROWS)
                .expect("default queued-work row bound is non-zero"),
            max_pending_age: Self::DEFAULT_MAX_PENDING_AGE,
            max_turn_input_admission: std::num::NonZeroUsize::new(
                Self::DEFAULT_MAX_TURN_INPUT_CLAIM,
            )
            .expect("default turn-input admission bound is non-zero"),
            drain_policy: None,
        }
    }

    /// Selects one of the two shipped drain shapes.
    ///
    /// Unset, Lash uses [`DrainMode::OneAtATime`](crate::DrainMode::OneAtATime):
    /// one row per run, queued work and next-turn host input alike, strict
    /// FIFO, no token arithmetic.
    pub fn with_drain_mode(mut self, mode: crate::DrainMode) -> Self {
        self.drain_policy = Some(crate::runtime::shared_drain_mode_policy(mode));
        self
    }

    /// This is the escape hatch for hosts wanting selection Lash deliberately
    /// does not ship, such as window-fitted prefix selection.
    pub fn with_drain_policy(
        mut self,
        drain_policy: std::sync::Arc<dyn crate::QueuedDrainPolicy>,
    ) -> Self {
        self.drain_policy = Some(drain_policy);
        self
    }

    pub fn drain_policy(&self) -> std::sync::Arc<dyn crate::QueuedDrainPolicy> {
        self.drain_policy
            .clone()
            .unwrap_or_else(crate::default_queued_drain_policy)
    }

    /// Sets the maximum number of compatible rows coalesced into one fresh
    /// admission. Redriving an interrupted admission may exceed this bound to preserve
    /// its already-journaled composition.
    ///
    /// # Panics
    ///
    /// Panics when `max_rows` is zero.
    pub const fn with_max_rows(mut self, max_rows: usize) -> Self {
        let Some(max_rows) = std::num::NonZeroUsize::new(max_rows) else {
            panic!("queued-work max rows must be non-zero");
        };
        self.max_rows = max_rows;
        self
    }

    /// Sets the age at which Lash admits the oldest compatible row alone
    /// instead of coalescing it with later ready rows.
    ///
    /// # Panics
    ///
    /// Panics when `max_pending_age` is zero.
    pub const fn with_max_pending_age(mut self, max_pending_age: std::time::Duration) -> Self {
        assert!(
            !max_pending_age.is_zero(),
            "queued-work max pending age must be non-zero"
        );
        self.max_pending_age = max_pending_age;
        self
    }

    /// Since FIG-1313 no shipped drain policy spends this budget: the two
    /// default modes do no token arithmetic. It is handed to the configured
    /// [`QueuedDrainPolicy`](crate::QueuedDrainPolicy) as
    /// [`QueuedDrainRequest::available_tokens`](crate::QueuedDrainRequest::available_tokens),
    /// where a custom policy may weigh it, and a reserve that consumes the
    /// whole model context is still rejected as a misconfiguration. A row
    /// larger than the entire context is refused and left pending.
    pub const fn action_token_reserve(&self) -> usize {
        self.action_token_reserve.get()
    }

    /// Sets the maximum number of pending next-turn inputs one idle admission
    /// offers the drain policy. How many of them the run takes is the drain
    /// policy's decision (ADR 0101 §5.2): the default takes one, so each
    /// input is its own run.
    ///
    /// Direct and drained ingress share the bound because they share the admission
    /// (ADR 0069): a direct turn takes the head of the same queue a drain does.
    /// A direct turn whose accepted input sits further back than this bound
    /// executes nothing and reports the input as queued; the drain answers it in
    /// arrival order.
    ///
    /// # Panics
    ///
    /// Panics when `max_inputs` is zero.
    pub const fn with_max_turn_input_admission(mut self, max_inputs: usize) -> Self {
        let Some(max_inputs) = std::num::NonZeroUsize::new(max_inputs) else {
            panic!("turn-input admission bound must be non-zero");
        };
        self.max_turn_input_admission = max_inputs;
        self
    }

    /// Returns the maximum number of pending next-turn inputs one idle admission
    /// offers the drain policy.
    pub const fn max_turn_input_admission(&self) -> usize {
        self.max_turn_input_admission.get()
    }

    /// Returns the maximum number of compatible rows in one fresh admission.
    ///
    /// This does not split an interrupted admission whose complete composition
    /// must be redriven atomically.
    pub const fn max_rows(&self) -> usize {
        self.max_rows.get()
    }

    /// Returns the age at which the oldest compatible row is admitted alone.
    ///
    /// This is a batching-latency bound, not a queue expiry: reaching it does
    /// not discard the row.
    pub const fn max_pending_age(&self) -> std::time::Duration {
        self.max_pending_age
    }

    pub fn admission_policy(&self, max_context_tokens: usize) -> TurnLaneAdmissionPolicy {
        TurnLaneAdmissionPolicy {
            max_context_tokens,
            action_token_reserve: self.action_token_reserve(),
            max_rows: self.max_rows(),
            max_pending_age_ms: u64::try_from(self.max_pending_age.as_millis()).unwrap_or(u64::MAX),
            drain_policy: self.drain_policy(),
        }
    }
}
