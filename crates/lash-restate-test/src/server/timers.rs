//! Virtual time: timers, their firing, and the inactivity timeout.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::Shared;
use super::model::{InvKey, Status, TimerAction};
use crate::protocol::MessageType;
use crate::protocol::generated::{self as pb, notification_template};

use super::processor::*;

/// Corresponding virtual and wall times at the last virtual-time move.
pub struct TimeAnchor {
    virtual_ms: u64,
    wall_us: u128,
    instant: Instant,
}

impl TimeAnchor {
    pub fn new(virtual_ms: u64) -> Self {
        let wall_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_micros())
            .unwrap_or(0);
        Self {
            virtual_ms,
            wall_us,
            instant: Instant::now(),
        }
    }

    pub fn wall_flowed_ms(&self) -> u64 {
        let elapsed = u64::try_from(self.instant.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.virtual_ms.saturating_add(elapsed)
    }

    /// Translate the SDK's absolute wall deadline without discarding time
    /// spent issuing or delivering its frame. Round up to the next virtual
    /// millisecond so the timer cannot fire before the stamped deadline.
    pub fn deadline_ms(&self, wall_epoch_ms: u64) -> u64 {
        let remaining_us = (u128::from(wall_epoch_ms) * 1000).saturating_sub(self.wall_us);
        let remaining_ms = u64::try_from(remaining_us.div_ceil(1000)).unwrap_or(u64::MAX);
        self.virtual_ms.saturating_add(remaining_ms)
    }
}

impl State {
    // ---------------------------------------------------------------------
    // Virtual time
    // ---------------------------------------------------------------------

    /// Add a timer. Timers due at one instant fire in an order the seed
    /// fixes from what each timer is — its invocation and completion — not
    /// from which handler happened to register first.
    pub(super) fn add_timer(&mut self, fire_at_ms: u64, action: TimerAction) {
        self.seq += 1;
        let (invocation, kind, detail) = match &action {
            TimerAction::Sleep {
                invocation,
                completion_id,
            } => (*invocation, &b"sleep"[..], *completion_id),
            TimerAction::Start { invocation } => (*invocation, &b"start"[..], 0),
            TimerAction::Retry { invocation } => (*invocation, &b"retry"[..], 0),
        };
        let invocation_id = self.invocations[invocation.0].id.bytes().clone();
        let tie_break = self
            .ids
            .derive(&[b"timer", &invocation_id, kind, &detail.to_be_bytes()])
            .1;
        self.timers
            .insert((fire_at_ms, tie_break, self.seq), action);
    }

    pub(super) fn remove_timers_of(&mut self, key: InvKey) {
        self.timers.retain(|_, action| match action {
            TimerAction::Sleep { invocation, .. }
            | TimerAction::Start { invocation }
            | TimerAction::Retry { invocation } => *invocation != key,
        });
    }

    pub(super) fn remove_retry_timer(&mut self, key: InvKey) {
        self.timers.retain(
            |_, action| !matches!(action, TimerAction::Retry { invocation } if *invocation == key),
        );
    }

    /// Whether auto-advance would fire something: a pending retry, or a
    /// timer due within `horizon_ms`.
    pub fn has_auto_timer(&self, horizon_ms: u64) -> bool {
        self.timers
            .values()
            .any(|action| matches!(action, TimerAction::Retry { .. }))
            || self
                .next_timer_ms()
                .is_some_and(|fire_at| fire_at <= self.now_ms.saturating_add(horizon_ms))
    }

    pub fn next_timer_ms(&self) -> Option<u64> {
        self.timers.keys().next().map(|(fire_at, _, _)| *fire_at)
    }

    /// Move virtual time to `target_ms`, firing every timer due on the way in
    /// time order, and close the input of attempts starved past their
    /// inactivity timeout.
    pub fn advance_to(&mut self, sh: &Arc<Shared>, target_ms: u64) -> usize {
        let mut fired = 0;
        while let Some((&(fire_at, tie_break, seq), _)) = self.timers.iter().next() {
            if fire_at > target_ms {
                break;
            }
            let Some(action) = self.timers.remove(&(fire_at, tie_break, seq)) else {
                break;
            };
            let from = self.now_ms;
            self.now_ms = self.now_ms.max(fire_at);
            if self.now_ms != from {
                self.anchor = TimeAnchor::new(self.now_ms);
            }
            sh.time_moved(self.now_ms);
            self.expire_inactive(sh, from);
            self.fire(sh, action);
            fired += 1;
        }
        let from = self.now_ms;
        self.now_ms = self.now_ms.max(target_ms);
        if self.now_ms != from {
            self.anchor = TimeAnchor::new(self.now_ms);
        }
        sh.time_moved(self.now_ms);
        self.expire_inactive(sh, from);
        self.stats.timers_fired += fired as u64;
        sh.activity.notify_waiters();
        fired
    }

    /// Start the earliest pending retry now, without moving virtual time: a
    /// compressed backoff. Returns whether one was pending.
    pub fn fire_next_retry(&mut self, sh: &Arc<Shared>) -> bool {
        let Some(key) = self
            .timers
            .iter()
            .find(|(_, action)| matches!(action, TimerAction::Retry { .. }))
            .map(|(key, _)| *key)
        else {
            return false;
        };
        if let Some(action) = self.timers.remove(&key) {
            self.fire(sh, action);
            self.stats.timers_fired += 1;
        }
        true
    }

    /// Fire the earliest timer, moving virtual time to it.
    pub fn fire_next(&mut self, sh: &Arc<Shared>) -> Option<u64> {
        let fire_at = self.next_timer_ms()?;
        let fired = self.advance_to(sh, fire_at.max(self.now_ms));
        (fired > 0).then_some(self.now_ms)
    }

    pub(super) fn fire(&mut self, sh: &Arc<Shared>, action: TimerAction) {
        match action {
            TimerAction::Sleep {
                invocation,
                completion_id,
            } => self.notify(
                sh,
                invocation,
                MessageType::SleepCompletionNotification,
                notification_template::Id::CompletionId(completion_id),
                notification_template::Result::Void(pb::Void {}),
            ),
            TimerAction::Start { invocation } => {
                if matches!(self.invocations[invocation.0].status, Status::Scheduled) {
                    self.enqueue(sh, invocation);
                }
            }
            TimerAction::Retry { invocation } => {
                if matches!(self.invocations[invocation.0].status, Status::BackingOff) {
                    self.start_attempt(sh, invocation);
                }
            }
        }
    }

    /// Close the input of every attempt starved for at least its inactivity
    /// timeout of virtual time, as `restate-server` does to an idle stream.
    pub(super) fn expire_inactive(&mut self, sh: &Arc<Shared>, from_ms: u64) {
        let now_ms = self.now_ms;
        let default_timeout = duration_ms(sh.config.inactivity_timeout);
        for invocation in &mut self.invocations {
            let timeout = invocation
                .spec
                .inactivity_timeout_ms
                .unwrap_or(default_timeout);
            if let Status::Running(attempt) = &mut invocation.status {
                if !attempt.is_open() || !attempt.probe.is_starved() {
                    attempt.starved_since_ms = None;
                    continue;
                }
                let since = *attempt.starved_since_ms.get_or_insert(from_ms);
                if now_ms.saturating_sub(since) >= timeout {
                    attempt.close();
                }
            }
        }
    }
}
pub fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::TimeAnchor;
    use std::time::Instant;

    #[test]
    fn sdk_deadlines_keep_the_wall_and_virtual_clock_offset() {
        let anchor = TimeAnchor {
            virtual_ms: 1_000,
            wall_us: 10_000_750,
            instant: Instant::now(),
        };
        for (stamp_ms, expected_ms) in [(10_400, 1_400), (10_025, 1_025), (11_234, 2_234)] {
            assert_eq!(anchor.deadline_ms(stamp_ms), expected_ms);
        }
    }

    #[test]
    fn sdk_deadlines_round_up_only_to_the_next_millisecond() {
        for (wall_us, expected_ms) in [
            (10_000_000, 1_400),
            (10_000_001, 1_400),
            (10_000_999, 1_400),
            (10_001_000, 1_399),
        ] {
            let anchor = TimeAnchor {
                virtual_ms: 1_000,
                wall_us,
                instant: Instant::now(),
            };
            assert_eq!(anchor.deadline_ms(10_400), expected_ms);
        }
    }

    #[test]
    fn sdk_deadlines_before_the_anchor_are_already_due() {
        let anchor = TimeAnchor {
            virtual_ms: 1_000,
            wall_us: 10_000_000,
            instant: Instant::now(),
        };
        assert_eq!(anchor.deadline_ms(10_000), 1_000);
        assert_eq!(anchor.deadline_ms(9_999), 1_000);
        assert_eq!(anchor.deadline_ms(0), 1_000);
    }

    #[test]
    fn sdk_deadlines_saturate_at_the_virtual_clock_limit() {
        let anchor = TimeAnchor {
            virtual_ms: u64::MAX - 1,
            wall_us: 0,
            instant: Instant::now(),
        };
        assert_eq!(anchor.deadline_ms(0), u64::MAX - 1);
        assert_eq!(anchor.deadline_ms(1), u64::MAX);
        assert_eq!(anchor.deadline_ms(u64::MAX), u64::MAX);
    }
}
