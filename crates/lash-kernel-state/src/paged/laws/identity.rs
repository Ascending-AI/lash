//! Long text and bytes identity (contract §6 "Long text identity"): every
//! key producer gives one key per content, by the frozen grammar of
//! `paged::identity`. Expected keys are golden values computed outside the
//! code under test, from the grammar's statement (r57/l2-text-identity.md):
//! SHA-256 over the tags, little-endian counts and child ids.
use super::heap::fresh;
use crate::arena::Arena;

use crate::paged::identity;
use lash_kernel_doc::heap::{NormalKey, PageReader, PageValue};

fn digest(hex: &str) -> NormalKey {
    NormalKey::Digest(bytes(hex).try_into().expect("a 32-byte digest"))
}

fn bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex"))
        .collect()
}

/// Texts and their golden keys: the 4096/4097 boundary, and the 1-, 2- and
/// 3-byte back-offs of a cut that would split a scalar.
fn texts() -> Vec<(String, NormalKey)> {
    vec![
        (
            "a".repeat(4096),
            digest("b8504b6dd73d8bfd40c25a823b785c58fe50e6a5b315392953255e1c6cb9d9e7"),
        ),
        (
            "a".repeat(4097),
            digest("455d48dd074c7f06b1d5be25db8933771281aa393a82ca56ba37eb8a749b7bba"),
        ),
        (
            "a".repeat(4095) + "é",
            digest("915496380ea872f0a3dc30016395aef3ea144e4e85326228c18ff35755581bd8"),
        ),
        (
            "a".repeat(4094) + "€",
            digest("c3b8e69ada0a1c26774374dfa5fa8aa4531569a8cd15edbf31686b017fe98092"),
        ),
        (
            "a".repeat(4093) + "😀",
            digest("de076753a9f9364d170d6ad8309d1a97730a96a9b2e73ccfc8e061c23afa5757"),
        ),
    ]
}

/// Bytes and their golden keys: the boundary, octets equal to a text's,
/// and one index of 32 leaves against two levels over 33.
fn octets() -> Vec<(Vec<u8>, NormalKey)> {
    vec![
        (
            vec![b'a'; 4096],
            digest("00692e84e5aca512191c3955e684ade6143bcc14bc3565f48835726534d4928e"),
        ),
        (
            vec![b'a'; 4097],
            digest("be1cc1c0a5f4914443d8ff8c3505d51398cec0e9bc23cdbaab6dd502628da1b0"),
        ),
        (
            vec![0x5a; 32 * 4096],
            digest("222faaf09f375a13e0914bd9f20f3520c8815be4777d7b7394d62c9e664bc23e"),
        ),
        (
            vec![0x5a; 32 * 4096 + 1],
            digest("5229fe0edd162dfe0f1b2d68e9fd70e809497340b426a9b420845989fac11639"),
        ),
    ]
}

/// A value keeps its flat key up to 4096 bytes and is keyed by its
/// identity tree's root above, the same whether the arena holds it
/// resident or a page heap holds it paged.
#[test]
fn a_long_value_is_keyed_by_its_identity_root_wherever_it_is_held() {
    let mut arena = Arena::new();
    let (heap, _pages) = fresh();
    for (text, expected) in texts() {
        assert_eq!(identity::text_key(&text), expected, "{} bytes", text.len());
        let resident = arena.text(&text);
        assert_eq!(
            arena.semantics().normal_key(&resident).expect("keyed"),
            Some(expected.clone()),
            "resident text of {} bytes",
            text.len()
        );
        let paged = PageValue::Text(heap.text(&text).expect("paged text"));
        assert_eq!(
            heap.normal_key(&paged).expect("keyed"),
            Some(expected),
            "paged text of {} bytes",
            text.len()
        );
    }
    for (octets, expected) in octets() {
        assert_eq!(
            identity::bytes_key(&octets),
            expected,
            "{} octets",
            octets.len()
        );
        let resident = arena.bytes(&octets);
        assert_eq!(
            arena.semantics().normal_key(&resident).expect("keyed"),
            Some(expected.clone()),
            "resident bytes of {} octets",
            octets.len()
        );
        let paged = PageValue::Bytes(heap.bytes(&octets).expect("paged bytes"));
        assert_eq!(
            heap.normal_key(&paged).expect("keyed"),
            Some(expected),
            "paged bytes of {} octets",
            octets.len()
        );
    }
}

/// A tuple embeds a long member by its identity root under the member tag
/// (`sum(11)`), in the arena and in a page heap alike: the paged heap once
/// embedded the member's raw text, so equal tuples keyed apart.
#[test]
fn a_tuple_embeds_a_long_member_by_its_identity_root() {
    let expected = NormalKey::Inline(bytes(
        "0c090109010c0b010820455d48dd074c7f06b1d5be25db8933771281aa393a82ca56ba37eb8a749b7bba",
    ));
    let text = "a".repeat(4097);
    let mut arena = Arena::new();
    let member = arena.text(&text);
    let tuple = arena.tuple(vec![member], 8).expect("tuple");
    assert_eq!(
        arena.semantics().normal_key(&tuple).expect("keyed"),
        Some(expected.clone())
    );
    let (heap, _pages) = fresh();
    let member = PageValue::Text(heap.text(&text).expect("paged text"));
    let tuple = PageValue::Tuple(heap.tuple(vec![member]).expect("tuple"));
    assert_eq!(heap.normal_key(&tuple).expect("keyed"), Some(expected));
}
