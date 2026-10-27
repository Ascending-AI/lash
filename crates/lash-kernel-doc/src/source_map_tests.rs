//! FIG-6373 / Design 1: checked source coordinates and injective rekeying.
use crate::Correspondence;
use crate::Document;
use crate::SourceLanguage;
use crate::SourceMap;
use crate::SourceMapRefusal;
use crate::SourceSite;
use crate::Span;
use crate::Survivor;
use crate::parse_document;
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;
use lash_kernel_value::tokens::decode_body;
use lash_kernel_value::tokens::encode_body;

fn document() -> Document {
    parse_document("kernel 1\nnumbers float\nmain { print 1 print 2 }").unwrap()
}
fn site(path: &[u32]) -> Site {
    Site::new(Unit::Main, path.to_vec())
}
fn location(path: &[u32], start: usize, end: usize) -> SourceSite {
    SourceSite {
        site: site(path),
        location: crate::SourceLocation::Spelled {
            ranges: crate::NonEmptySpans::new(Span::new(start, end).unwrap()),
        },
    }
}
fn map() -> SourceMap {
    SourceMap::new(
        &document(),
        "é12",
        SourceLanguage::TypeScript,
        vec![location(&[1], 3, 4), location(&[0], 0, 2)],
    )
    .unwrap()
}

/// Every validation dimension is checked, with document and digest first.
#[test]
fn validation_checks_identity_sites_utf8_offsets_and_order() {
    let document = document();
    let map = map();
    map.validate(&document, "é12").unwrap();
    let mut invalid = map.clone();
    invalid.document = parse_document("kernel 1\nnumbers float\nmain {}")
        .unwrap()
        .identity();
    assert_eq!(
        invalid.validate(&document, "bad"),
        Err(SourceMapRefusal::Document)
    );
    assert_eq!(
        map.validate(&document, "bad"),
        Err(SourceMapRefusal::Digest)
    );
    invalid = map.clone();
    invalid.sites[0].site = site(&[99]);
    assert!(matches!(
        invalid.validate(&document, "é12"),
        Err(SourceMapRefusal::Site { .. })
    ));
    for range in [
        Span { start: 1, end: 2 },
        Span { start: 2, end: 1 },
        Span { start: 0, end: 5 },
    ] {
        invalid = map.clone();
        invalid.sites[0].location = crate::SourceLocation::Spelled {
            ranges: crate::NonEmptySpans::new(range),
        };
        assert!(matches!(
            invalid.validate(&document, "é12"),
            Err(SourceMapRefusal::Range { .. })
        ));
    }
    for offset in [1, 5] {
        invalid = map.clone();
        invalid.sites[0].location = crate::SourceLocation::Implicit { offset };
        assert!(matches!(
            invalid.validate(&document, "é12"),
            Err(SourceMapRefusal::Implicit { .. })
        ));
    }
    invalid = map.clone();
    invalid.sites.reverse();
    assert!(matches!(
        invalid.validate(&document, "é12"),
        Err(SourceMapRefusal::Order { .. })
    ));
    assert!(
        SourceMap::new(
            &document,
            "é12",
            SourceLanguage::Python,
            vec![location(&[0], 0, 2), location(&[0], 2, 3)]
        )
        .is_err()
    );
    let implicit = SourceSite {
        site: site(&[]),
        location: crate::SourceLocation::Implicit { offset: 4 },
    };
    SourceMap::new(&document, "é12", SourceLanguage::Python, vec![implicit]).unwrap();
}

/// Rekey drops removed sites, reorders moved sites and keeps edited ranges.
#[test]
fn rekey_retains_edited_survivors_and_refuses_another_base() {
    let map = map();
    let identity = Correspondence::identity(
        map.document,
        map.sites.iter().map(|entry| entry.site.clone()),
    )
    .unwrap();
    assert_eq!(map.rekey(&identity), Some(map.clone()));
    let result = parse_document("kernel 1\nnumbers float\nmain { print 3 print 4 }").unwrap();
    let correspondence = Correspondence::of(
        map.document,
        result.identity(),
        vec![
            Survivor::new(site(&[0]), site(&[1]), true),
            Survivor::new(site(&[1]), site(&[0]), false),
        ],
    )
    .unwrap();
    let rekeyed = map.rekey(&correspondence).unwrap();
    rekeyed.validate(&result, "é12").unwrap();
    assert_eq!(rekeyed.sites[1].location, map.sites[0].location);
    assert_eq!(rekeyed.sites[0].location, map.sites[1].location);
    let removal = Correspondence::of(
        rekeyed.document,
        map.document,
        vec![Survivor::new(site(&[1]), site(&[0]), false)],
    )
    .unwrap();
    assert_eq!(rekeyed.rekey(&removal).unwrap().sites.len(), 1);
    assert_eq!(map.rekey(&removal), None);
    assert_eq!(
        map.rekey(&correspondence.then(&removal).unwrap()),
        rekeyed.rekey(&removal)
    );
}

/// JSON and tokens refuse noncanonical maps and noninjective correspondence.
#[test]
fn canonical_encodings_preserve_checked_invariants() {
    let map = map();
    assert_eq!(decode_body::<SourceMap>(&encode_body(&map)).unwrap(), map);
    assert_eq!(
        serde_json::from_str::<SourceMap>(&serde_json::to_string(&map).unwrap()).unwrap(),
        map
    );
    let mut reversed = map.clone();
    reversed.sites.reverse();
    assert!(decode_body::<SourceMap>(&encode_body(&reversed)).is_err());
    assert!(serde_json::from_value::<SourceMap>(serde_json::to_value(&reversed).unwrap()).is_err());
    let mut json = serde_json::to_value(&map).unwrap();
    json["extra"] = true.into();
    assert!(serde_json::from_value::<SourceMap>(json).is_err());
    assert!(Span::new(2, 1).is_none());
    assert!(serde_json::from_str::<Span>(r#"{"start":2,"end":1}"#).is_err());
    assert!(decode_body::<Span>(&encode_body(&Span { start: 2, end: 1 })).is_err());
    let correspondence = Correspondence::identity(map.document, [site(&[0]), site(&[1])]).unwrap();
    assert_eq!(
        decode_body::<Correspondence>(&encode_body(&correspondence)).unwrap(),
        correspondence
    );
    assert_eq!(
        serde_json::from_str::<Correspondence>(&serde_json::to_string(&correspondence).unwrap())
            .unwrap(),
        correspondence
    );
    for (from, to) in [(site(&[0]), site(&[1])), (site(&[1]), site(&[0]))] {
        let mut entries = correspondence.entries().to_vec();
        entries[1] = Survivor::new(from, to, false);
        assert!(Correspondence::of(map.document, map.document, entries.clone()).is_none());
        let json = serde_json::json!({"base":map.document,"result":map.document,"entries":entries});
        assert!(serde_json::from_value::<Correspondence>(json).is_err());
    }
    let mut json = serde_json::to_value(&correspondence).unwrap();
    json["entries"].as_array_mut().unwrap().reverse();
    assert_eq!(
        serde_json::from_value::<Correspondence>(json).unwrap(),
        correspondence
    );
}
