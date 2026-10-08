use super::materialization::{encoding_allowance, materialization_cost};

#[test]
fn expansion_overflow_is_refused() {
    assert!(encoding_allowance(u64::MAX, 0).is_none());
    assert!(materialization_cost(4, u64::MAX, 0).is_none());
}
