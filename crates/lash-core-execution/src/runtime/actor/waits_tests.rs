//! The key MAC is HMAC-SHA256 as RFC 4231 specifies it: the one primitive
//! this module implements itself.

use super::{hex, hmac_sha256};

#[test]
fn the_key_mac_is_rfc_4231_hmac_sha256() {
    // RFC 4231 §4.3 (a key shorter than the block) and §4.6 (a key longer
    // than the block, hashed first).
    assert_eq!(
        hex(&hmac_sha256(
            b"Jefe",
            &[b"what do ya want ", b"for nothing?"]
        )),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert_eq!(
        hex(&hmac_sha256(
            &[0xaa; 131],
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
        )),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}
