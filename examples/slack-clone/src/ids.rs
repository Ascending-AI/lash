//! Slack-shaped identifiers and message timestamps.
//!
//! Slack's ids are opaque prefixed strings (`C…` channel, `U…` user, `B…` bot,
//! `T…` team, `A…` app, `Ev…` event) and a message's identity is its `ts` — a
//! decimal string of epoch seconds with a six-digit fractional part, unique
//! within a channel. Downstream code that treats `ts` as "just a timestamp" and
//! reformats it breaks message identity, so this module keeps the string form
//! canonical and the numeric form (microseconds) private to parsing/ordering.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Slack-ish id alphabet: uppercase alphanumerics with the visually ambiguous
/// `I`, `L`, `O` and `U` removed. Real Slack ids are opaque, so any stable
/// alphabet is faithful as long as the prefix and length are.
const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Length of the random part of an id. Slack ids are typically 9-11 characters
/// after the prefix; the platform mints a fixed 9.
const ID_BODY_LEN: usize = 9;

/// Mints prefixed, Slack-shaped ids.
///
/// Sequential rather than random: a monotonic counter mixed into the encoded
/// body keeps ids unique within a run, and the seed keeps them unique across
/// runs. Tests construct one with a fixed seed to get a stable id sequence.
#[derive(Debug)]
pub struct IdMinter {
    counter: AtomicU64,
}

impl IdMinter {
    /// A minter seeded from the wall clock — the production constructor.
    pub fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or_default();
        Self::seeded(seed)
    }

    /// A minter with an explicit seed, so a test can pin the id sequence.
    pub fn seeded(seed: u64) -> Self {
        Self {
            counter: AtomicU64::new(seed),
        }
    }

    /// Mint the next id with the given prefix (`"C"`, `"U"`, `"B"`, `"Ev"`, …).
    pub fn mint(&self, prefix: &str) -> String {
        let ticket = self.counter.fetch_add(1, Ordering::Relaxed);
        // Splitmix64 finalizer: spreads adjacent counter values across the
        // encoded body so consecutive ids do not look sequential.
        let mut mixed = ticket.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^= mixed >> 31;

        let mut id = String::with_capacity(prefix.len() + ID_BODY_LEN);
        id.push_str(prefix);
        for slot in 0..ID_BODY_LEN {
            let shift = (slot * 5) % 60;
            let index = ((mixed >> shift) & 0x1f) as usize;
            id.push(ALPHABET[index] as char);
        }
        id
    }
}

impl Default for IdMinter {
    fn default() -> Self {
        Self::new()
    }
}

/// A Slack message timestamp: epoch microseconds, rendered as `"<secs>.<micros>"`.
///
/// This is message *identity*, not a display timestamp. `Ord` follows the
/// numeric value so `ts` ordering matches conversation ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ts(u64);

impl Ts {
    /// The current wall-clock instant as a `ts`.
    pub fn now() -> Self {
        Self(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_micros() as u64)
                .unwrap_or_default(),
        )
    }

    /// Construct from raw epoch microseconds.
    pub fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// Raw epoch microseconds — the sortable/paginable form.
    pub fn micros(self) -> u64 {
        self.0
    }

    /// The next distinct `ts`, used to break a same-microsecond collision.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Parse Slack's wire form. Accepts a bare integer second count (Slack's
    /// `oldest`/`latest` bounds allow it) as well as the fractional form.
    pub fn parse(raw: &str) -> Option<Self> {
        let (seconds, fraction) = match raw.split_once('.') {
            Some((seconds, fraction)) => (seconds, fraction),
            None => (raw, ""),
        };
        if seconds.is_empty() || !seconds.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let seconds: u64 = seconds.parse().ok()?;
        // Right-pad/truncate the fraction to microsecond precision.
        let mut micros_text = String::with_capacity(6);
        micros_text.push_str(fraction.get(..6).unwrap_or(fraction));
        while micros_text.len() < 6 {
            micros_text.push('0');
        }
        let micros: u64 = micros_text.parse().ok()?;
        Some(Self(seconds.checked_mul(1_000_000)?.checked_add(micros)?))
    }

    /// Epoch seconds, for the `event_time` envelope field and `created` fields.
    pub fn epoch_seconds(self) -> u64 {
        self.0 / 1_000_000
    }
}

impl std::fmt::Display for Ts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}.{:06}",
            self.0 / 1_000_000,
            self.0 % 1_000_000
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_renders_slacks_seconds_dot_microseconds_form() {
        assert_eq!(
            Ts::from_micros(1_503_435_956_000_247).to_string(),
            "1503435956.000247"
        );
    }

    #[test]
    fn ts_round_trips_through_its_wire_form() {
        let ts = Ts::from_micros(1_512_085_950_000_216);
        assert_eq!(Ts::parse(&ts.to_string()), Some(ts));
    }

    #[test]
    fn ts_parses_the_bare_second_bounds_slack_accepts() {
        assert_eq!(
            Ts::parse("1512085950"),
            Some(Ts::from_micros(1_512_085_950_000_000))
        );
        assert_eq!(Ts::parse("not-a-ts"), None);
    }

    #[test]
    fn minted_ids_carry_the_prefix_and_are_distinct() {
        let minter = IdMinter::seeded(7);
        let first = minter.mint("C");
        let second = minter.mint("C");
        assert!(first.starts_with('C'));
        assert_eq!(first.len(), 1 + ID_BODY_LEN);
        assert_ne!(first, second);
        assert!(minter.mint("Ev").starts_with("Ev"));
    }
}
