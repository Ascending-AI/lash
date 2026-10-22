// A complete Document declares Imports and an optional Main. Statements have no semicolon.
fn document_example() {
    use lash::workflow::document::{ParseErrorReason, parse_document, print_document};
    let text = "kernel 1\nnumbers float\nmain {\n  return \"inbox\"\n}\n";
    let document=parse_document(text).expect("a complete workflow parses");
    let reparsed=parse_document(&print_document(&document)).expect("canonical text parses");
    assert_eq!(reparsed.identity(),document.identity());
    let error=parse_document(&text.replace("return \"inbox\"","return \"inbox\";")).unwrap_err();
    assert!(matches!(error.reason,ParseErrorReason::Malformed {..}));
    assert_eq!((error.line,error.column),(4,17));
}
