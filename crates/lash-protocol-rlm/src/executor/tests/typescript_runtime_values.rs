//! FIG-3079: the TypeScript dialect's journaled clock and RNG inside durable
//! processes.

/// FIG-3079: a journaled float must decode to the double that was written.
///
/// serde_json's default float parser is a fast approximation that lands one
/// ULP off for roughly a tenth of all doubles; the workspace therefore pins
/// the `float_roundtrip` feature. Without it every journaled `Math.random()`
/// sample has about a one-in-ten chance of replaying as a different number,
/// and so does every other float that crosses an effect journal.
#[test]
pub(super) fn journaled_floats_decode_to_the_double_that_was_written() {
    let mut mismatches = Vec::new();
    for index in 0..50_000u64 {
        // The same construction the journaled `random` executor uses: the low
        // 53 bits of a draw, mapped onto the unit interval.
        let bits = ((u128::from(index) * 0x9E37_79B9_7F4A_7C15_u128) & ((1_u128 << 53) - 1)) as u64;
        let written = bits as f64 / ((1_u64 << 53) as f64);
        let encoded = serde_json::to_string(&written).expect("encode a journaled float");
        let decoded: f64 = serde_json::from_str(&encoded).expect("decode a journaled float");
        if decoded != written {
            mismatches.push(encoded);
        }
    }
    assert!(
        mismatches.is_empty(),
        "journaled floats must replay exactly; {} of 50000 drifted, e.g. {:?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(3)]
    );
}
