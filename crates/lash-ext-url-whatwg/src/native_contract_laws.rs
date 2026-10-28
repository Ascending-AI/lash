//! FIG-7532: omitted native holdings and scratch fail the shared allocation law.
#[path = "../../lash-kernel-lib/src/native_contract_observer.rs"]
mod allocation_observer;
#[path = "../../lash-kernel-lib/src/native_scratch_observer.rs"]
pub(crate) mod observer;
pub(crate) use observer::observe;

#[derive(Default)]
pub(crate) struct ObservedString(String);
impl url::form_urlencoded::Target for ObservedString {
    type Finished = String;
    fn as_mut_string(&mut self) -> &mut String {
        observe(16 + self.0.len() as u64);
        &mut self.0
    }
    fn finish(mut self) -> String {
        observe(16 + self.0.len() as u64);
        std::mem::take(&mut self.0)
    }
}
impl Drop for ObservedString {
    fn drop(&mut self) {
        observe(16 + self.0.len() as u64);
    }
}

use lash_kernel_doc::Datum;
fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}
fn check(name: &str, input: Vec<Datum>) {
    let native = super::native(
        name,
        lash_kernel_value::names::FunctionId::from_bytes([0x5a; 32]),
    )
    .unwrap();
    assert!(
        allocation_observer::check_case(&*native, name, input, observer::reset, observer::peak)
            .is_some(),
        "{name} uncertified"
    );
}
/// The URL/live-params corpus checks shared state identities, percent expansion,
/// malformed pairs and raised exits after an accumulated serialization prefix.
#[test]
fn native_contracts_match_logical_holdings_and_reject_understatements() {
    let mut checked = 0;
    for (input, base) in [
        ("https://example.test/?a=1", Datum::Absent),
        ("/p?a=1&a=2", text("https://example.com")),
        ("not a url", Datum::Absent),
        ("/x", text("https://example.com")),
        ("/x", text("not a url")),
        ("https://example.com", Datum::Null),
    ] {
        check("url.whatwg.parse", vec![text(input), base]);
        checked += 1;
    }
    for (field, value) in [
        ("href", "/p?a=1&a=2"),
        ("protocol", "http:"),
        ("username", "hello world"),
        ("password", "p&ss"),
        ("hostname", "example.test"),
        ("host", "example.test:8080"),
        ("host", "bad host"),
        ("port", ""),
        ("port", "8080"),
        ("port", "invalid"),
        ("pathname", "/p"),
        ("search", "?c=3"),
        ("hash", "#x"),
        ("unknown", "x"),
    ] {
        check(
            "url.whatwg.update",
            vec![
                text("https://example.com/p?a=1&a=2"),
                text(field),
                text(value),
            ],
        );
        checked += 1;
    }
    for input in [
        "",
        "?a=1&a=2&b=hello+world",
        "?c=4",
        "x=%FF&x=%F0%9F%98%80&empty&=",
        "&&&",
    ] {
        check("url.whatwg.query_parse", vec![text(input)]);
        checked += 1;
    }
    let pair = |name, value| Datum::List(vec![text(name), text(value)]);
    for input in [
        vec![],
        vec![pair("a", "1"), pair("a", "2"), pair("b", "hello world")],
        vec![pair("x", "😀%&")],
        vec![Datum::Null],
        vec![Datum::List(vec![])],
        vec![Datum::List(vec![Datum::Null, text("x")])],
        vec![Datum::List(vec![text("x")])],
        vec![Datum::List(vec![text("x"), Datum::Null])],
        vec![pair("b", "hello world"), Datum::Null],
    ] {
        check("url.whatwg.query_stringify", vec![Datum::List(input)]);
        checked += 1;
    }
    let query = "a=1&a=2&b=hello+world&".repeat(65);
    check("url.whatwg.query_parse", vec![text(&query)]);
    checked += 1;
    check(
        "url.whatwg.parse",
        vec![
            text(&format!("https://example.com/p?{query}")),
            Datum::Absent,
        ],
    );
    checked += 1;
    for operation in super::Operation::ALL {
        check(operation.name(), vec![Datum::Null]);
        check(operation.name(), vec![]);
        checked += 2;
    }
    eprintln!("checked {checked} URL input/exit contracts");
}
