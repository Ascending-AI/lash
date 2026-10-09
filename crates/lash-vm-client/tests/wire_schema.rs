#![expect(
    clippy::expect_used,
    reason = "wire schema laws parse declared source fixtures"
)]
//! Snapshot Serde declarations directly from syntax, without enumerating API names.
//! Field order, field types, variant forms, Serde attributes and feature gates
//! are all wire inputs. Both feature selections have committed snapshots.
use quote::ToTokens;
use syn::{Attribute, Fields, Item};

fn enabled(attributes: &[Attribute], testing: bool) -> bool {
    attributes
        .iter()
        .filter(|attr| attr.path().is_ident("cfg"))
        .all(|attr| {
            assert_eq!(
                attr.meta.to_token_stream().to_string(),
                "cfg (feature = \"testing\")",
                "unhandled wire feature gate"
            );
            testing
        })
}

fn serde_attributes(attributes: &mut Vec<Attribute>) {
    attributes.retain(|attr| attr.path().is_ident("serde"));
}

fn fields(fields: &mut Fields) {
    match fields {
        Fields::Named(named) => named.named.iter_mut().for_each(|field| {
            serde_attributes(&mut field.attrs);
            field.vis = syn::Visibility::Inherited;
        }),
        Fields::Unnamed(unnamed) => unnamed.unnamed.iter_mut().for_each(|field| {
            serde_attributes(&mut field.attrs);
            field.vis = syn::Visibility::Inherited;
        }),
        Fields::Unit => {}
    }
}

fn serializable(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attr| {
        attr.path().is_ident("derive")
            && attr
                .meta
                .to_token_stream()
                .to_string()
                .split(|ch: char| !ch.is_alphanumeric())
                .any(|part| part == "Serialize")
    })
}

fn declarations(source: &str, testing: bool) -> Vec<String> {
    syn::parse_file(source)
        .expect("wire source parses")
        .items
        .into_iter()
        .filter_map(|item| match item {
            Item::Enum(mut item) if serializable(&item.attrs) && enabled(&item.attrs, testing) => {
                serde_attributes(&mut item.attrs);
                item.vis = syn::Visibility::Inherited;
                item.variants = item
                    .variants
                    .into_iter()
                    .filter(|variant| enabled(&variant.attrs, testing))
                    .map(|mut variant| {
                        serde_attributes(&mut variant.attrs);
                        fields(&mut variant.fields);
                        variant
                    })
                    .collect();
                Some(item.to_token_stream().to_string())
            }
            Item::Struct(mut item)
                if serializable(&item.attrs) && enabled(&item.attrs, testing) =>
            {
                serde_attributes(&mut item.attrs);
                item.vis = syn::Visibility::Inherited;
                fields(&mut item.fields);
                Some(item.to_token_stream().to_string())
            }
            _ => None,
        })
        .collect()
}

fn check_snapshot(testing: bool) {
    let mut schema = format!(
        "worker protocol {}\nframe header {} bytes\ntesting {}\n\n",
        lash_vm_protocol::WORKER_PROTOCOL_VERSION - u32::from(cfg!(feature = "synthetic-next")),
        lash_vm_protocol::FRAME_HEADER_BYTES,
        testing
    );
    for source in [
        include_str!("../src/service.rs"),
        include_str!("../src/wire.rs"),
    ] {
        for declaration in declarations(source, testing) {
            schema.push_str(&declaration);
            schema.push_str("\n\n");
        }
    }
    schema.truncate(schema.trim_end().len());
    schema.push('\n');
    let expected = if testing {
        include_str!("snapshots/wire-v1-testing.snap")
    } else {
        include_str!("snapshots/wire-v1.snap")
    };
    if schema != expected {
        println!("BEGIN_WIRE_SCHEMA\n{schema}END_WIRE_SCHEMA");
    }
    assert_eq!(
        schema, expected,
        "wire shape changed: review the change and refresh the snapshot in place before 1.0; after 1.0 bump WORKER_PROTOCOL_VERSION and retain the old versioned snapshot"
    );
}

#[test]
fn base_wire_schema_matches_committed_snapshot() {
    check_snapshot(false);
}

#[cfg(feature = "testing")]
#[test]
fn testing_wire_schema_matches_committed_snapshot() {
    check_snapshot(true);
}
