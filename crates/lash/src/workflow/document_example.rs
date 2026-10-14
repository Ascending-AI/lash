// A whole workflow document: the `kernel` and `numbers` header lines, an
// `entry` signature for each declared function a host may start, and
// `main`. Statements end at the line; kernel text has no `;`.
fn document_example() {
    use lash::workflow::document::{ParseErrorReason, parse_document, print_document};

    let text = r#"kernel 1
numbers float
entry digest(inbox: Text) -> Text

fn digest(inbox) {
  return inbox
}

main {
  finish null
}
"#;

    let document = parse_document(text).expect("a whole workflow parses");

    // Printing is canonical: the printed text parses to a document with the
    // same identity.
    let printed = print_document(&document);
    let reparsed = parse_document(&printed).expect("the printed text parses");
    assert_eq!(reparsed.identity().unwrap(), document.identity().unwrap());

    // A trailing `;` is a typed refusal naming its line and column.
    let misspelled = text.replace("return inbox", "return inbox;");
    let error = parse_document(&misspelled).unwrap_err();
    assert!(matches!(error.reason, ParseErrorReason::Malformed { .. }));
    assert_eq!((error.line, error.column), (6, 15));
}
