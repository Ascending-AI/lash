//! Virtual time: timers, their firing, and the inactivity timeout.

use std::sync::Arc;
use std::time::Duration;

use super::Shared;
use super::model::{InvKey, Status, TimerAction};
use crate::protocol::MessageType;
use crate::protocol::generated::{self as pb, notification_template};

use super::processor::*;

impl State {
    // ---------------------------------------------------------------------
    // Virtual time
    // ---------------------------------------------------------------------

    pub(super) fn add_timer(&mut self, fire_at_ms: u64, action: TimerAction) {
        self.seq += 1;
        let tie_break = self.ids.next_u64();
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
            sh.time_moved(self.now_ms);
            self.expire_inactive(sh, from);
            self.fire(sh, action);
            fired += 1;
        }
        let from = self.now_ms;
        self.now_ms = self.now_ms.max(target_ms);
        self.anchor = (self.now_ms, std::time::Instant::now());
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
                if attempt.input.is_none() || !attempt.probe.is_starved() {
                    attempt.starved_since_ms = None;
                    continue;
                }
                let since = *attempt.starved_since_ms.get_or_insert(from_ms);
                if now_ms.saturating_sub(since) >= timeout {
                    attempt.input = None;
                }
            }
        }
    }
}
pub fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The SDK stamps sleeps and delayed sends with wall-clock epoch
/// milliseconds (`SystemTime::now() + duration`, truncated). The server
/// keeps virtual time, so it recovers the duration the handler asked for by
/// measuring the stamp against the wall clock when the command arrives, and
/// re-anchors it on the virtual clock.
///
/// Truncation and the frame's transit make the measured value read short of
/// the requested duration by up to [`DURATION_SNAP_WINDOW_US`]. Within that
/// window the server picks the roundest candidate (a whole second, then a
/// multiple of 100 ms, 10 ms, 1 ms), so the durations handlers use recover
/// exactly and one seed fires timers in one order.
pub fn wall_delay_ms(wall_epoch_ms: u64) -> u64 {
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros())
        .unwrap_or(0);
    let target_us = u128::from(wall_epoch_ms) * 1000;
    snap_duration_ms(target_us.saturating_sub(now_us))
}

/// How far short of the requested duration a measured one may read.
pub const DURATION_SNAP_WINDOW_US: u128 = 2_000;

fn snap_duration_ms(measured_us: u128) -> u64 {
    for granularity_us in [1_000_000_u128, 100_000, 10_000, 1_000] {
        let candidate = measured_us.div_ceil(granularity_us) * granularity_us;
        if candidate <= measured_us + DURATION_SNAP_WINDOW_US {
            return u64::try_from(candidate / 1000).unwrap_or(u64::MAX);
        }
    }
    u64::try_from(measured_us.div_ceil(1000)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::snap_duration_ms;

    #[test]
    fn measured_durations_snap_to_what_the_handler_asked_for() {
        assert_eq!(snap_duration_ms(59_999_050), 60_000);
        assert_eq!(snap_duration_ms(59_998_200), 60_000);
        assert_eq!(snap_duration_ms(24_100), 25);
        assert_eq!(snap_duration_ms(99_300), 100);
        assert_eq!(snap_duration_ms(1_234_000), 1_234);
        assert_eq!(snap_duration_ms(0), 0);
    }
}
