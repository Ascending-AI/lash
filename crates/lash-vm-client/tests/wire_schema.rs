//! Independent vectors for the custom capability encoding at the carry boundary.
use lash_kernel_doc::Datum;
use lash_protocol_types::engine_fault::PageRefusal;
use lash_vm_client::wire::{CarryWire, PageLink, StartWire};
use lash_vm_protocol::page::{PageAuthority, PageCapability, PageReply};
use serde_json::json;

// Reviewed 1.0 bytes: page 00..1f, kind, little-endian authority and epoch,
// MAC e0..ff. Neither vector is produced by the codec under test.
const ROOT: &str = concat!(
    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    "00",
    "ffffffffffffffff",
    "ffffffffffffffff",
    "e0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff"
);
const STAGE: &str = concat!(
    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    "01",
    "0807060504030201",
    "1817161514131211",
    "e0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff"
);

fn capability(authority: PageAuthority, epoch: u64) -> PageCapability {
    PageCapability {
        page: std::array::from_fn(|i| i as u8),
        authority,
        epoch,
        mac: std::array::from_fn(|i| 224 + i as u8),
    }
}

#[test]
fn page_and_carry_wire_vectors_preserve_the_1_0_contract() -> Result<(), Box<dyn std::error::Error>>
{
    let root = capability(PageAuthority::Root(u64::MAX), u64::MAX);
    let stage = capability(
        PageAuthority::Stage(0x0102_0304_0506_0708),
        0x1112_1314_1516_1718,
    );
    for (value, hex) in [(root, ROOT), (stage, STAGE)] {
        assert_eq!(serde_json::to_value(value)?, json!(hex));
        assert_eq!(serde_json::from_value::<PageCapability>(json!(hex))?, value);
        // MessagePack bin8, length 81, followed by the exact same packed bytes.
        let mut bytes = vec![0xc4, 81];
        for i in (0..hex.len()).step_by(2) {
            bytes.push(u8::from_str_radix(&hex[i..i + 2], 16)?);
        }
        assert_eq!(rmp_serde::to_vec(&value)?, bytes);
        assert_eq!(rmp_serde::from_slice::<PageCapability>(&bytes)?, value);
    }
    let start = StartWire {
        entry: Some("next".into()),
        args: vec![Datum::Bool(true)],
        session: Some(CarryWire {
            root: vec![0, 128, 255],
            pages: vec![
                PageLink::Granted(root),
                PageLink::Granted(stage),
                PageLink::Carried([7; 32]),
            ],
        }),
        reads: vec![],
        host: vec![],
    };
    let prior = json!({
        "entry": "next", "args": [{"bool": true}],
        "session": {"root": [0, 128, 255], "pages": [
            {"granted": ROOT}, {"granted": STAGE}, {"carried": vec![7; 32]}
        ]}
    });
    assert_eq!(serde_json::to_value(&start)?, prior);
    assert_eq!(serde_json::from_value::<StartWire>(prior)?, start);
    for (refusal, prior) in [
        (
            PageRefusal::Unauthorized,
            json!({"refused": "unauthorized"}),
        ),
        (
            PageRefusal::BatchTooLarge {
                pages: u64::MAX,
                bytes: u64::MAX,
            },
            json!({"refused": {"batch_too_large": {"pages": u64::MAX, "bytes": u64::MAX}}}),
        ),
    ] {
        let reply = PageReply::Refused(refusal);
        assert_eq!(serde_json::to_value(&reply)?, prior);
        assert_eq!(serde_json::from_value::<PageReply>(prior)?, reply);
    }
    Ok(())
}
