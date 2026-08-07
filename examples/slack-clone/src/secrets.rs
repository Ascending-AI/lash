//! Constant-time comparison for the shared secrets both processes check.
//!
//! Both sides compare an attacker-supplied string against a configured one — the
//! platform a bearer token, the bot an event envelope's verification token. `==`
//! on `str` short-circuits at the first differing byte, so the time it takes to
//! reject leaks how much of the prefix was right. That is a real (if slow) oracle
//! against a network attacker, and an example that models credential checking
//! should not model it wrongly.

/// Compare two secrets without leaking a match prefix through timing.
///
/// Length is folded into the accumulator rather than returned early, so a wrong
/// length is not distinguishable from wrong bytes. Reference implementations
/// would reach for `subtle`; this is written out because the arithmetic is short
/// and the property is the point of reading it.
pub fn constant_time_eq(presented: &str, expected: &str) -> bool {
    let presented = presented.as_bytes();
    let expected = expected.as_bytes();
    let mut difference = (presented.len() ^ expected.len()) as u32;
    // Walk the expected length, so the number of iterations depends only on the
    // configured secret and never on what an attacker sent. Reading past the end
    // of a short input folds in a zero rather than returning early; the length
    // term above is what actually rejects it.
    for (index, expected_byte) in expected.iter().enumerate() {
        let presented_byte = presented.get(index).copied().unwrap_or(0);
        difference |= u32::from(presented_byte ^ expected_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_secrets_compare_equal() {
        assert!(constant_time_eq("slack-clone-token", "slack-clone-token"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn differing_secrets_compare_unequal_at_every_position() {
        assert!(!constant_time_eq("slack-clone-token", "slack-clone-tokeN"));
        assert!(!constant_time_eq("Xlack-clone-token", "slack-clone-token"));
        assert!(!constant_time_eq("slack-clone-token", ""));
        assert!(!constant_time_eq("", "slack-clone-token"));
    }

    #[test]
    fn a_prefix_does_not_compare_equal_to_a_longer_secret() {
        // The length term has to do real work: `ab` matches every byte of
        // `abab` that it has, and must still be rejected.
        assert!(!constant_time_eq("ab", "abab"));
        assert!(!constant_time_eq("abab", "ab"));
    }
}
