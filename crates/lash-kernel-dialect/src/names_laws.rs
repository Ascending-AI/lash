use lash_kernel_value::EffectName;

use crate::{SpellKind, Spelling};
use {
    lash_kernel_value::MemberPath, lash_kernel_value::PackagePath,
    lash_kernel_value::QualifiedOperation, lash_kernel_value::Segment,
};

// Exhaust all short words over representative letter, digit, separator,
// uppercase and non-ASCII classes; long fixtures exercise multiple humps.
fn texts() -> Vec<String> {
    let mut texts = vec![String::new()];
    let mut level = vec![String::new()];
    for _ in 0..4 {
        level = level
            .iter()
            .flat_map(|prefix| ['a', 'b', '0', '1', '_', 'A', 'é'].map(|c| format!("{prefix}{c}")))
            .collect();
        texts.extend(level.iter().cloned());
    }
    texts.extend(
        [
            "send_item",
            "get2fa_code",
            "get_2fa_code",
            "a__b",
            "sendItem",
            "a1b",
            "utf8_decoder",
            "a_b_c",
            "data_quanta",
            "a1_b2_c3",
            "mail.send_item",
            "send_item.now",
            "a_",
            "_a",
            "a\n",
            "a-b",
            "a b",
            "a\0b",
            "SendItem",
            "Get2faCode",
            "Utf8Decoder",
            "URL",
            "aBC",
        ]
        .map(str::to_owned),
    );
    texts
}

/// NAME-CANON: construction and decoding admit exactly the canonical grammar;
/// package paths are nonempty and every member holds validated segments.
#[test]
fn name_canon() {
    for text in texts() {
        // An independent state machine for [a-z][a-z0-9]*(_[a-z][a-z0-9]*)*.
        let mut letter_required = true;
        let mut valid = true;
        for c in text.chars() {
            if letter_required {
                valid &= c.is_ascii_lowercase();
                letter_required = false;
            } else if c == '_' {
                letter_required = true;
            } else {
                valid &= c.is_ascii_lowercase() || c.is_ascii_digit();
            }
        }
        valid &= !letter_required;
        assert_eq!(Segment::is_canonical(&text), valid, "{text:?}");
        assert_eq!(Segment::new(&text).is_ok(), valid, "{text:?}");
        let decoded = serde_json::from_value::<Segment>(serde_json::json!(text));
        assert_eq!(decoded.is_ok(), valid, "{text:?}");
    }
    assert!(PackagePath::try_from(Vec::new()).is_err());
    for text in [
        "",
        ".mail",
        "mail.",
        "mail..inbox",
        "mail.Bad",
        "get_2fa.code",
    ] {
        assert!(PackagePath::parse(text).is_err());
        assert!(serde_json::from_value::<PackagePath>(serde_json::json!(text)).is_err());
    }
    let path = PackagePath::parse("regex.ecma").unwrap();
    assert_eq!(path.to_string(), "regex.ecma");
    assert_eq!(serde_json::to_value(&path).unwrap(), "regex.ecma");
    assert_eq!(
        serde_json::from_value::<PackagePath>(serde_json::json!("regex.ecma")).unwrap(),
        path
    );
    let paths = std::collections::BTreeMap::from([(path, true)]);
    let json = serde_json::to_value(&paths).unwrap();
    assert_eq!(json, serde_json::json!({"regex.ecma": true}));
    assert_eq!(
        serde_json::from_value::<std::collections::BTreeMap<PackagePath, bool>>(json).unwrap(),
        paths
    );
    let member = MemberPath {
        export: Segment::new("data_quanta").unwrap(),
        member: Some(Segment::new("reduce_by_key").unwrap()),
    };
    for member in [
        member,
        MemberPath {
            export: Segment::new("data_quanta").unwrap(),
            member: None,
        },
    ] {
        let text = member.to_string();
        assert_eq!(
            text,
            if member.member.is_some() {
                "data_quanta.reduce_by_key"
            } else {
                "data_quanta"
            }
        );
        assert_eq!(serde_json::to_value(&member).unwrap(), text);
        let members = std::collections::BTreeMap::from([(member, true)]);
        let json = serde_json::to_value(&members).unwrap();
        assert_eq!(
            serde_json::from_value::<std::collections::BTreeMap<MemberPath, bool>>(json).unwrap(),
            members
        );
    }
    for text in [
        "",
        ".sort",
        "data_quanta.",
        "data_quanta..sort",
        "data_quanta.sort.now",
        "data_quanta.Sort",
    ] {
        assert!(serde_json::from_value::<MemberPath>(serde_json::json!(text)).is_err());
    }
}

/// NAME-SPELL: both directions are inverses on their domains, and the member
/// and type namespaces cannot collide in either dialect.
#[test]
fn name_spell() {
    let texts = texts();
    let canonical: Vec<_> = texts
        .iter()
        .filter_map(|text| Segment::new(text).ok())
        .collect();
    for spelling in [Spelling::TypeScript, Spelling::Python] {
        let mut members = std::collections::BTreeSet::new();
        let mut types = std::collections::BTreeSet::new();
        for kind in [
            SpellKind::PathSegment,
            SpellKind::Member,
            SpellKind::TypeName,
        ] {
            let mut seen = std::collections::BTreeMap::new();
            for segment in &canonical {
                let text = spelling.spell(kind, segment);
                assert_eq!(spelling.unspell(kind, &text).as_ref(), Some(segment));
                if let Some(previous) = seen.insert(text.clone(), segment) {
                    assert_eq!(previous, segment);
                }
                match kind {
                    SpellKind::Member => {
                        members.insert(text);
                    }
                    SpellKind::TypeName => {
                        types.insert(text);
                    }
                    SpellKind::PathSegment => {
                        assert_eq!(text, segment.as_str());
                    }
                }
            }
            for text in &texts {
                if let Some(segment) = spelling.unspell(kind, text) {
                    assert_eq!(spelling.spell(kind, &segment), *text);
                }
            }
        }
        assert!(members.is_disjoint(&types));
        for (canonical, camel, pascal) in [
            ("send_item", "sendItem", "SendItem"),
            ("a1b", "a1b", "A1b"),
            ("utf8_decoder", "utf8Decoder", "Utf8Decoder"),
            ("get2fa_code", "get2faCode", "Get2faCode"),
        ] {
            let segment = Segment::new(canonical).unwrap();
            assert_eq!(spelling.spell(SpellKind::TypeName, &segment), pascal);
            assert_eq!(
                spelling.spell(SpellKind::Member, &segment),
                if spelling == Spelling::TypeScript {
                    camel
                } else {
                    canonical
                }
            );
        }
    }
    assert_eq!(
        Spelling::TypeScript.unspell(SpellKind::Member, "send_item"),
        None
    );
}

/// OP-SPLIT: the last dot recovers every operation's sole package/name split;
/// dotted names and empty packages cannot enter through construction or serde.
#[test]
fn op_split() {
    let segments: Vec<_> = ["mail", "send_item", "a1b", "utf8_decoder", "get2fa_code"]
        .map(|text| Segment::new(text).unwrap())
        .into();
    for first in &segments {
        for second in &segments {
            for name in &segments {
                for path in [vec![first.clone()], vec![first.clone(), second.clone()]] {
                    let operation = QualifiedOperation {
                        package: PackagePath::try_from(path).unwrap(),
                        name: name.clone(),
                    };
                    assert_eq!(
                        QualifiedOperation::from_effect(&operation.effect_name()).unwrap(),
                        operation
                    );
                    let value = serde_json::to_value(&operation).unwrap();
                    assert_eq!(
                        serde_json::from_value::<QualifiedOperation>(value).unwrap(),
                        operation
                    );
                }
            }
        }
    }
    let operation =
        QualifiedOperation::from_effect(&EffectName::new("mail.send_item").unwrap()).unwrap();
    assert_eq!(
        operation.package.as_slice(),
        [Segment::new("mail").unwrap()]
    );
    assert_eq!(operation.name.as_str(), "send_item");
    assert_eq!(operation.effect_name().as_str(), "mail.send_item");
    for effect in [
        "send_item",
        "Mail.send_item",
        "mail.sendItem",
        "mail._send",
        "mail.get_2fa",
    ] {
        assert!(QualifiedOperation::from_effect(&EffectName::new(effect).unwrap()).is_err());
    }
    for value in [
        serde_json::json!({"package": "mail", "name": "send_item.now"}),
        serde_json::json!({"package": "", "name": "send_item"}),
        serde_json::json!({"package": "mail.Bad", "name": "send_item"}),
    ] {
        assert!(serde_json::from_value::<QualifiedOperation>(value).is_err());
    }
}
