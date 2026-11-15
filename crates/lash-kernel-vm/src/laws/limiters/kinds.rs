use super::{Drive, drive, inlined, reference};
use crate::laws::embedder::text as datum_text;

/// K-CHG-002 and K-BND-003: intersected kind guards preserve each branch,
/// completion event and resource refusal, including the final default.
#[test]
fn kind_guard_intersections_keep_the_late_default_and_each_other_branch() {
    let text = r#"main {
  for xs in [absent, null, true, 0, 1.5, "s", [], map{}, set{}, (), ("token",), {}, fn() { return 1 }, &law.kind_switch] {
    try { print invoke law.kind_switch(xs) } catch e { print "caught" }
  }
  return null
}"#;
    let whole = inlined(text, Drive::default());
    let expected = [
        "absent", "null", "bool", "number", "number", "text", "list", "map", "set", "caught",
        "token", "caught", "closure", "closure",
    ];
    assert_eq!(whole.printed, expected.map(datum_text));
    let before = crate::vm::inline::held();
    let late = drive(
        "main { return invoke law.kind_switch(absent) }",
        Drive::default(),
    );
    assert!(late.end.contains("absent"), "{late:?}");
    assert!(
        crate::vm::inline::held() > before,
        "the late kind arm did not fold"
    );
    for cold in [false, true] {
        for exact in [false, true] {
            for slice in [u64::MAX, 1, 7] {
                let how = Drive {
                    cold,
                    exact,
                    slice,
                    ..Drive::default()
                };
                assert_eq!(inlined(text, how), reference(text, how));
            }
        }
        for limit in 0..=whole.charged.unwrap() {
            let how = Drive {
                cold,
                limit,
                ..Drive::default()
            };
            assert_eq!(inlined(text, how), reference(text, how));
        }
        for memory in (800..16000).step_by(137) {
            let how = Drive {
                cold,
                memory,
                ..Drive::default()
            };
            assert_eq!(inlined(text, how), reference(text, how));
        }
    }
}

pub(super) fn register(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    registered: &mut Vec<(&'static str, lash_kernel_value::FunctionId)>,
) {
    for (name, source, native) in [
        (
            "law.kind",
            "function law.kind(xs: Any) -> Text\nkernel 1\ncharge 1\nnative\n",
            Some("kind"),
        ),
        (
            "law.same",
            "function law.same(a: Any, b: Any) -> Bool\nkernel 1\ncharge 1\nnative\n",
            Some("same"),
        ),
        (
            "law.kind_switch",
            "function law.kind_switch(xs: Any) -> Text\nkernel 1\ncharge sum(3, size(result))\n{uses}body { let k = law.kind(xs) if law.same(k, \"text\") { return k } if law.same(k, \"list\") { return k } if law.same(k, \"record\") { return xs[999] } if law.same(k, \"float\") { return \"number\" } if law.same(k, \"integer\") { return \"number\" } if law.same(k, \"function\") { return \"closure\" } if law.same(k, \"tuple\") { return xs[0] } return k }",
            None,
        ),
    ] {
        let uses: String = registered
            .iter()
            .map(|(name, id)| format!("use {name} = @{id}\n"))
            .collect();
        let definition =
            lash_kernel_doc::parse_definition(&source.replace("{uses}", &uses)).unwrap();
        let native = native.map(|name| lash_kernel_lib::native(name).unwrap());
        registered.push((name, registry.register(definition, native).unwrap()));
    }
}
