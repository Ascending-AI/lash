// A Document declares signature-only Imports; fulfilment chooses their fills.
// Main is optional and executes only for StartSelector::Main.
fn document_example() {
    use lash_kernel_doc::{parse_document, print_document};

    let text = "kernel 1\nnumbers float\nimport 0 operation service.echo(value: Text) -> Text\nmain { let answer = perform 0(\"hello\") as Text finish answer }\n";
    let document = parse_document(text).expect("a complete Document");
    let reparsed = parse_document(&print_document(&document)).expect("canonical Document text");
    assert_eq!(reparsed.identity(), document.identity());
}
