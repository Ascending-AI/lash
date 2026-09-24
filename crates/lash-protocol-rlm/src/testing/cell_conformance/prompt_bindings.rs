//! Axis 6: the prompt's "Bound Variables" section lists every binding a later
//! cell can name (FIG-3629).
//!
//! A binding with no host view — a TypeScript exotic, or a record holding one —
//! is still a session global, so the section lists it by a bounded,
//! console-style summary. Checked resident and across a reload, since a reload
//! is exactly when a model most needs to be told what survived.

use super::harness::{HarnessMode, Session};

const BIND_EVERY_KIND: &str = r#"const seen = new Map([["a", 1], ["b", { deep: [1, 2] }]]);
const tags = new Set(["x", "y"]);
const when = new Date(Date.UTC(2020, 0, 2, 3, 4, 5));
const pattern = /a(b+)/g;
const site = new URL("https://example.com/a?x=1");
const query = new URLSearchParams("a=1&b=2");
const holder = { m: new Map(), n: 1 };
const plain = { n: 1 };
const big = new Map();
for (let i = 0; i < 50; i++) {
  big.set(i, "x".repeat(100));
}"#;

#[test]
fn the_bound_variables_section_lists_every_kind_of_binding() {
    for mode in HarnessMode::ALL {
        let mut session = Session::open(*mode);
        session.run_ok(BIND_EVERY_KIND);
        session.run_ok(r#"pattern.exec("abbxab");"#);
        let prompt = session.bound_variables_prompt();
        for line in [
            r#"- `seen`: Map(2) {"a" => 1, "b" => { deep: […] }}"#,
            r#"- `tags`: Set(2) {"x", "y"}"#,
            "- `when`: Date(2020-01-02T03:04:05.000Z)",
            "- `pattern`: /a(b+)/g (lastIndex 3)",
            r#"- `site`: URL("https://example.com/a?x=1")"#,
            r#"- `query`: URLSearchParams("a=1&b=2")"#,
            "- `holder`: { m: Map(0) {}, n: 1 }",
        ] {
            assert!(
                prompt.lines().any(|candidate| candidate == line),
                "{mode:?}: the section must list `{line}`:\n{prompt}"
            );
        }
        assert!(
            prompt.lines().any(|line| line.starts_with("- `plain`")),
            "{mode:?}: a binding with a host view keeps its ordinary row:\n{prompt}"
        );
        let big = prompt
            .lines()
            .find(|line| line.starts_with("- `big`: Map(50) {0 => "))
            .unwrap_or_else(|| panic!("{mode:?}: the large Map is listed:\n{prompt}"));
        assert!(
            big.trim_start_matches("- `big`: ").chars().count()
                <= lashlang::BINDING_SUMMARY_MAX_CHARS,
            "{mode:?}: a summary is bounded however large its value: {big}"
        );
    }
}
