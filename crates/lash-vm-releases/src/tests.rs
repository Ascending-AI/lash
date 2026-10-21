//! L-HELPER-REACH: release packages point at self-contained helper code.
use super::*;
#[path = "helper_classes.rs"]
mod helper_classes;

#[test]
fn helper_exports_retain_one_closed_document_and_native_declarations() {
    let release = helper_release().expect("the build's helper release assembles");
    assert_eq!(release.content.documents.len(), 1);
    assert_eq!(release.content.links.len(), 1);
    let (link_id, link) = release
        .content
        .links
        .iter()
        .next()
        .expect("the helper Link");
    let document = release
        .content
        .documents
        .get(&link.document)
        .expect("its Document");
    assert_eq!(link.fills.len(), document.imports.len());
    for package in release.content.packages.values() {
        for member in package.members.values() {
            let MemberKind::Export { link, export } = &member.kind else {
                panic!("helper code exports only code")
            };
            assert_eq!(link, link_id);
            assert!(document.exports.contains_key(&export.name));
        }
    }
    for (function, definition) in &document.functions {
        assert!(release.functions.get(function).is_some());
        if let Some(body) = definition.body() {
            for called in body.functions.keys() {
                assert!(
                    document.functions.contains_key(called),
                    "{called} is document-local"
                );
            }
            for (slot, import) in &body.imports {
                assert_eq!(document.imports.get(slot), Some(import));
            }
        }
    }
}
