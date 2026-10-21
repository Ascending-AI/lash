//! JSON-LABELS: one strict document preserves display facts without identity.

use std::collections::BTreeMap;

use super::kernel;
use crate::MemberDisplay;
use crate::Package;
use crate::PackageDisplay;
use crate::PackageRefusal;
use crate::PackageSources;
use crate::Root;
use crate::testing::{DATAFRAME, RELEASE, dataframe};
use lash_vocabulary::names::MemberPath;
use lash_vocabulary::names::PackagePath;

/// JSON-LABELS-PRESERVE: mechanically converted labels retain exactly the
/// former TSV values. Counts and FNV-1a digests were captured before deletion,
/// over sorted (name, title, comparison-or-empty, reply-bit), NUL terminated.
#[test]
fn json_labels_preserve_the_fixture_and_every_shipped_display_map() {
    let kernel = kernel();
    let fixture = dataframe(&kernel).unwrap();
    let expected = [
        ("col", "column"),
        ("lit", "literal"),
        ("count", "count rows"),
        ("expr.name", "expression name"),
        ("data_quanta.flat_map", "flat map"),
        ("data_quanta.collect_rows", "collect rows"),
    ]
    .into_iter()
    .map(|(key, title)| {
        (
            MemberPath::try_from(key.to_owned()).unwrap(),
            MemberDisplay::new(title),
        )
    })
    .collect();
    assert_eq!(
        fixture.display(),
        &PackageDisplay {
            members: expected,
            definitions: BTreeMap::new()
        }
    );
    for (path, root, count, expected) in [
        ("kernel", "", 310, 0x1bf45b0ef3379f16),
        ("typescript", "ts", 746, 0xd0d6bcc57da3212a),
        ("python", "py", 120, 0x2a91bf5aa65ca6be),
        ("regex.ecma", "regex.ecma", 6, 0x1a57ebd5d8741f47),
        ("date.ecma", "date.ecma", 4, 0xb46d64fbe20275eb),
        ("url.whatwg", "url.whatwg", 4, 0xf4852b71720eb24b),
    ] {
        let package = if path == "kernel" {
            kernel.clone()
        } else {
            Package::lash(
                PackagePath::parse(path).unwrap(),
                "1.0",
                [Root::new(root)],
                &[],
                |_| Ok(Vec::new()),
            )
            .unwrap()
        };
        let display = crate::shipped_display::of(&package).unwrap();
        assert!(display.members.is_empty());
        assert_eq!(display.definitions.len(), count, "{path}");
        let mut digest = 0xcbf29ce484222325_u64;
        for (name, value) in display.definitions {
            for token in [
                name.as_str(),
                &value.title,
                &value
                    .comparison
                    .map(|kind| format!("{kind:?}"))
                    .unwrap_or_default(),
                if value.reply { "1" } else { "0" },
            ] {
                for byte in token.bytes().chain([0]) {
                    digest = (digest ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                }
            }
        }
        assert_eq!(digest, expected, "{path}");
    }
}

/// JSON-LABELS-REFUSE: invalid labels retain the offending key and JSON pointer
/// in a typed refusal, including duplicate keys at every object depth.
#[test]
fn invalid_json_labels_name_the_offending_key_and_pointer() {
    let kernel = kernel();
    for (text, pointer, key) in [
        (
            r#"{"members":{"missing":"title"}}"#,
            "/members/missing",
            "missing",
        ),
        (
            r#"{"members":{"col":{"title":"column","extra":42}}}"#,
            "/members/col/extra",
            "extra",
        ),
        (r#"{"extra":42}"#, "/extra", "extra"),
        (r#"[{"col":"title"},{}]"#, "", ""),
        (
            r#"{"members":{"col":["title",null,false]}}"#,
            "/members/col",
            "col",
        ),
        (
            r#"{"members":{"col":"one","col":"two"}}"#,
            "/members/col",
            "col",
        ),
        (r#"{"members":{},"members":{}}"#, "/members", "members"),
        (
            r#"{"members":{"col":{"title":"one","title":"two"}}}"#,
            "/members/col/title",
            "title",
        ),
        (
            r#"{"members":{"col":{"title":"one","a~/b":false}}}"#,
            "/members/col/a~0~1b",
            "a~/b",
        ),
        (r#"{"members":{"col":"  "}}"#, "/members/col", "col"),
        (
            r#"{"definitions":{"other.helper":"title"}}"#,
            "/definitions/other.helper",
            "other.helper",
        ),
        (
            r#"{"definitions":{"dataframe.col":"title"}}"#,
            "/definitions/dataframe.col",
            "dataframe.col",
        ),
    ] {
        let refusal = Package::build(
            "dataframe",
            RELEASE,
            &PackageSources {
                labels: text,
                ..DATAFRAME
            },
            &[&kernel],
        )
        .unwrap_err();
        let PackageRefusal::Labels { error, .. } = &refusal else {
            panic!("{refusal:?}")
        };
        assert_eq!(error.pointer, pointer, "{text}");
        assert_eq!(error.key, key, "{text}");
        assert!(refusal.to_string().contains(pointer));
        assert!(!refusal.to_string().contains("line "));
    }
}

/// JSON-LABELS-IDENTITY: changing display facts changes neither the artifact
/// digest nor any function identity, including generated constructors/checks.
#[test]
fn changing_json_labels_keeps_the_digest_and_every_function_identity() {
    let kernel = kernel();
    let original = dataframe(&kernel).unwrap();
    let changed = Package::build("dataframe", RELEASE, &PackageSources {
        labels: r#"{"members":{"col":{"title":"a \"column\"\n列","comparison":"Eq","reply":true}},"definitions":{"dataframe.make_expr":"private constructor"}}"#,
        ..DATAFRAME
    }, &[&kernel]).unwrap();
    assert_ne!(original.display(), changed.display());
    assert_eq!(original.digest(), changed.digest());
    let identities = |package: &Package| {
        (0..package.code().len())
            .map(|at| (package.code().name(at).to_owned(), package.code().ids()[at]))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(identities(&original), identities(&changed));
    let display = &changed.display().members[&MemberPath::try_from("col".to_owned()).unwrap()];
    assert_eq!(display.title, "a \"column\"\n列");
    assert_eq!(display.comparison, Some(crate::Comparison::Eq));
    assert!(display.reply);
}
