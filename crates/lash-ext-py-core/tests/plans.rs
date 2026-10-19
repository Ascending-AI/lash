//! The checked-in plans and manifest are what the helper compiler gives
//! for the current helper bodies: a body edited without regenerating them
//! fails here, as drifted generated BUCK files fail the build.

use lash_ext_helper_compiler::library::Dialect;

#[test]
fn plans_are_what_the_compiler_gives_for_the_current_bodies() {
    let generated =
        lash_ext_helper_compiler::generate(Dialect::Python).expect("the helpers compile");
    let stale = "is stale: run `kiln run //crates/lash-ext-helper-compiler:regenerate__bin`";
    assert!(
        generated.rust == include_str!("../src/generated/plans.rs"),
        "src/generated/plans.rs {stale}"
    );
    assert!(
        generated.manifest == include_str!("../src/generated/plans.tsv"),
        "src/generated/plans.tsv {stale}"
    );
}

/// FIG-6715: closed try bodies, declining callees and closure factories
/// are compiled, rather than silently left interpreted.
#[test]
fn closed_helpers_have_generated_plans() {
    let generated =
        lash_ext_helper_compiler::generate(Dialect::Python).expect("the helpers compile");
    for name in [
        "py.range",
        "py.round",
        "py.chr",
        "py.method.insert",
        "py.method.clear",
    ] {
        let row = generated
            .manifest
            .lines()
            .find(|line| line.starts_with(&format!("{name}\t")))
            .expect("the helper is admitted");
        assert!(row.ends_with("\tgenerated"), "{row}");
    }
    for row in generated.manifest.lines().skip(2) {
        if row.split('\t').nth(2) == Some("closed") {
            assert!(row.ends_with("\tgenerated"), "{row}");
        }
    }
}

/// What a heap path asked of the heap: its table lookups and its element
/// reads, each a probe of a table.
#[derive(Default)]
struct Probes {
    lookups: usize,
    reads: usize,
    /// What a lookup finds: `None` where the table holds no key.
    found: Option<lash_kernel_doc::Value>,
}

impl lash_kernel_doc::plan::TemplateHeap for Probes {
    fn call(&mut self, _: usize, _: &[lash_kernel_doc::Value]) -> Option<lash_kernel_doc::Value> {
        None
    }
    fn bind(&mut self, _: lash_kernel_doc::Value, _: Option<lash_kernel_doc::Value>) -> Option<()> {
        Some(())
    }
    fn path(&mut self) {}
    fn index(
        &mut self,
        _: lash_kernel_doc::Value,
        _: lash_kernel_doc::Value,
    ) -> Option<lash_kernel_doc::Value> {
        self.reads += 1;
        self.found
    }
    fn field(&mut self, _: lash_kernel_doc::Value, _: &str) -> Option<lash_kernel_doc::Value> {
        self.reads += 1;
        self.found
    }
    fn tuple(&mut self, _: &[lash_kernel_doc::Value]) -> Option<lash_kernel_doc::Value> {
        None
    }
    fn set_index(
        &mut self,
        _: lash_kernel_doc::Value,
        _: lash_kernel_doc::Value,
        _: lash_kernel_doc::Value,
    ) -> Option<()> {
        None
    }
    fn len(
        &mut self,
        _: lash_kernel_doc::plan::Collection,
        _: lash_kernel_doc::Value,
    ) -> Option<lash_kernel_doc::Value> {
        None
    }
    fn lookup(
        &mut self,
        _: lash_kernel_doc::plan::Table,
        _: lash_kernel_doc::Value,
        _: lash_kernel_doc::Value,
    ) -> Option<Option<lash_kernel_doc::Value>> {
        self.lookups += 1;
        Some(self.found)
    }
}

/// FIG-6846: each level of `tree["level"]["rows"]` looks its key up once.
/// `py.getitem`'s heap path on a dict, or a record at a text key, takes
/// the entry its one lookup found; at a missing key it declines, and the
/// body raises the KeyError.
#[test]
fn a_subscript_looks_its_key_up_once() {
    use lash_kernel_doc::Value;
    use lash_kernel_doc::arena::{ArenaHandle, ShortText};
    let getitem = lash_ext_py_core::PLANS
        .iter()
        .find(|plan| plan.name == "py.getitem")
        .expect("py.getitem has a plan");
    let heap = getitem
        .template
        .as_ref()
        .and_then(|template| template.heap.as_ref())
        .expect("py.getitem has heap paths");
    let key = Value::Text(ShortText::new("rows").expect("a short text"));
    for target in [
        Value::Map(ArenaHandle::new(0, 0)),
        Value::Record(ArenaHandle::new(0, 0)),
    ] {
        for found in [Some(Value::Int(7)), None] {
            let mut probes = Probes {
                found,
                ..Probes::default()
            };
            let answer = (heap.entry)(&mut probes, &[target, key], &[]);
            assert!(
                match found {
                    Some(_) => matches!(answer, Some(Value::Int(7))),
                    None => answer.is_none(),
                },
                "{target:?} at a key found: {found:?} answers {answer:?}"
            );
            assert_eq!(
                (probes.lookups, probes.reads),
                (1, 0),
                "{target:?} at a key found: {found:?}"
            );
        }
    }
}
