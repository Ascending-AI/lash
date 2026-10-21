//! PKG-SHAPE: a member called with a value of a package type's brand and
//! another shape raises `package.stale_value`, naming the type, the path,
//! the value's constructor and the check, whose artifacts' labels the
//! message names, with a short digest when the labels are equal; a value
//! of another brand raises `type_error`.

use std::sync::Arc;

use lash_kernel_doc::ErrorDatum;
use lash_kernel_doc::FunctionRegistry;
use lash_vocabulary::legacy_names::FunctionId;

use super::{SHOUT, finished, functions, kernel, kernel_with, literal, raised, run, runtime};
use crate::testing::{DATAFRAME, RELEASE, dataframe};
use crate::{HeldPackages, Package, PackageSources, STALE_VALUE, StaleValue};

const MEMBERS: &[&str] = &[
    "dataframe.col",
    "dataframe.expr.name",
    "dataframe.data_quanta.read_text",
    "dataframe.data_quanta.filter",
    "dataframe.data_quanta.flat_map",
    "dataframe.data_quanta.reduce_by_key",
    "dataframe.data_quanta.limit",
];

/// An `expr` of the fixture's brand whose record lacks its `name`.
const STALE: &str = "{op: \"col\", args: [], title: \"x\", __lash_brand: (\"dataframe\", \"expr\"), __lash_built: b\"0000000000000000000000000000000000000000000000000000000000000000\"}";

struct Fixture {
    built: Package,
    registry: Arc<FunctionRegistry>,
    members: Vec<FunctionId>,
    held: HeldPackages,
}

fn fixture() -> Fixture {
    let kernel = kernel();
    let built = dataframe(&kernel).unwrap();
    let registry = runtime(&[&built]);
    let members = functions(&registry, &[&built], MEMBERS);
    let held = HeldPackages::install([kernel, built.clone()]).unwrap();
    Fixture {
        built,
        registry,
        members,
        held,
    }
}

impl Fixture {
    fn raise(&self, main: &str) -> ErrorDatum {
        raised(run(&self.registry, &self.members, main))
    }

    fn stale(&self, main: &str) -> StaleValue {
        let error = self.raise(main);
        assert_eq!(error.kind, STALE_VALUE, "{error:?}");
        let stale = StaleValue::of(&error).expect("a stale value's data");
        assert_eq!(stale.ty, "dataframe.expr");
        assert_eq!(
            Some(stale.current),
            self.built.function("dataframe.expr.check_shape"),
            "the check names itself"
        );
        stale
    }
}

#[test]
fn pkg_shape_a_stale_value_is_found_wherever_its_slot_holds_it() {
    let fixture = fixture();
    let quanta = format!(
        "let q = invoke dataframe.data_quanta.read_text(\"a b\")\nlet good = invoke dataframe.col(\"a\")\nlet stale = {STALE}\nlet lambda = fn(line) {{ return [line] }}"
    );

    let top = fixture.stale(&format!(
        "{quanta}\nlet q2 = invoke dataframe.data_quanta.filter(q, stale)\nreturn q2"
    ));
    assert_eq!(top.at, "predicate.name");
    let message = top.message(&fixture.held);
    assert!(
        message.starts_with("`dataframe.expr` at `predicate.name` was built by another release of `dataframe` (an unknown release 000000000000; current 0.3.0 "),
        "{message}"
    );

    let in_list = fixture.stale(&format!(
        "{quanta}\nlet keys = [good, stale]\nlet none = []\nlet q2 = invoke dataframe.data_quanta.reduce_by_key(q, keys, none)\nreturn q2"
    ));
    assert_eq!(in_list.at, "keys[].name");

    let in_options = fixture.stale(&format!(
        "{quanta}\nlet options = {{output: \"utf8\", where: stale}}\nlet q2 = invoke dataframe.data_quanta.flat_map(q, lambda, options)\nreturn q2"
    ));
    assert_eq!(in_options.at, "options.where.name");

    let in_representation = fixture.stale(&format!(
        "{quanta}\nlet q2 = {{source: \"a b\", plan: [{{step: \"filter\", exprs: [stale]}}], __lash_brand: (\"dataframe\", \"data_quanta\"), __lash_built: b\"00\"}}\nlet q3 = invoke dataframe.data_quanta.limit(q2, 5)\nreturn q3"
    ));
    assert_eq!(in_representation.at, "self.plan[].exprs[].name");

    // A well-shaped chain passes every check, the user's lambda as given.
    let chain = finished(run(
        &fixture.registry,
        &fixture.members,
        &format!(
            "{quanta}\nlet options = {{output: \"utf8\", where: good}}\nlet q2 = invoke dataframe.data_quanta.flat_map(q, lambda, options)\nlet q3 = invoke dataframe.data_quanta.filter(q2, good)\nlet q4 = invoke dataframe.data_quanta.limit(q3, 5)\nlet name = invoke dataframe.expr.name(good)\nreturn (name, q4.plan[0].step)"
        ),
    ));
    assert_eq!(literal(&chain), "(\"a\", \"flat_map\")");
}

/// A value built under another artifact of the line with the same label,
/// over another kernel, names both artifacts by label and short digest;
/// under another label, by label alone.
#[test]
fn pkg_shape_a_stale_value_names_both_artifacts_by_label() {
    let fixture = fixture();
    let older_kernel = kernel_with(&[SHOUT]);
    let older_surface = DATAFRAME.surface.replace(
        "type expr = Record{op: Text, args: List(Any), name: Text}",
        "type expr = Record{op: Text, args: List(Any), title: Text}",
    );
    let older_source = DATAFRAME
        .kernel
        .replace(
            "{op: op, args: args, name: name,",
            "{op: op, args: args, title: name,",
        )
        .replace("return self.name", "return self.title");
    for (release, short) in [(RELEASE, true), ("0.2.0", false)] {
        let older = Package::build(
            "dataframe",
            release,
            &PackageSources {
                kernel: &older_source,
                surface: &older_surface,
                ..DATAFRAME
            },
            &[&older_kernel],
        )
        .unwrap();
        let registry = runtime(&[&older]);
        let col = functions(&registry, &[&older], &["dataframe.col"]);
        let value = finished(run(
            &registry,
            &col,
            "let made = invoke dataframe.col(\"x\")\nreturn made",
        ));
        let stale = fixture.stale(&format!(
            "let value = {}\nlet name = invoke dataframe.expr.name(value)\nreturn name",
            literal(&value)
        ));
        assert_eq!(stale.built, older.function("dataframe.make_expr"));
        assert_eq!(stale.at, "self.name");

        let known: Vec<&Package> = fixture
            .held
            .artifacts()
            .values()
            .map(AsRef::as_ref)
            .chain([&older_kernel, &older])
            .collect();
        let message = stale.message_among(&known);
        let expected = if short {
            format!(
                "({release} {}; current {RELEASE} {})",
                older.digest().short(),
                fixture.built.digest().short()
            )
        } else {
            format!("({release}; current {RELEASE})")
        };
        assert_eq!(
            message,
            format!(
                "`dataframe.expr` at `self.name` was built by another release of `dataframe` {expected}; rebuild it"
            )
        );
    }
}

/// A constructor two held releases share, unchanged, cannot tell which of
/// them built a value: the message names both, and picks neither.
#[test]
fn pkg_shape_a_constructor_two_releases_share_names_both() {
    let fixture = fixture();
    let other_kernel = kernel_with(&[SHOUT]);
    let sharing = Package::build("dataframe", "0.2.9", &DATAFRAME, &[&other_kernel]).unwrap();
    assert_eq!(
        sharing.function("dataframe.make_expr"),
        fixture.built.function("dataframe.make_expr"),
        "unchanged source keeps its constructor"
    );
    let reshaped = Package::build(
        "dataframe",
        "0.4.0",
        &PackageSources {
            kernel: &DATAFRAME
                .kernel
                .replace(
                    "{op: op, args: args, name: name,",
                    "{op: op, args: args, title: name,",
                )
                .replace("return self.name", "return self.title"),
            surface: &DATAFRAME.surface.replace(
                "type expr = Record{op: Text, args: List(Any), name: Text}",
                "type expr = Record{op: Text, args: List(Any), title: Text}",
            ),
            ..DATAFRAME
        },
        &[&other_kernel],
    )
    .unwrap();
    let value = finished(run(
        &fixture.registry,
        &fixture.members,
        "let made = invoke dataframe.col(\"x\")\nreturn made",
    ));
    let registry = runtime(&[&reshaped]);
    let name = functions(&registry, &[&reshaped], &["dataframe.expr.name"]);
    let error = raised(run(
        &registry,
        &name,
        &format!(
            "let value = {}\nlet name = invoke dataframe.expr.name(value)\nreturn name",
            literal(&value)
        ),
    ));
    let stale = StaleValue::of(&error).expect("a stale value's data");
    let known: Vec<&Package> = fixture
        .held
        .artifacts()
        .values()
        .map(AsRef::as_ref)
        .chain([&other_kernel, &sharing, &reshaped])
        .collect();
    let mut both = [&fixture.built, &sharing];
    both.sort_by_key(|package| package.digest());
    assert_eq!(
        stale.message_among(&known),
        format!(
            "`dataframe.expr` at `self.title` was built by another release of `dataframe` (one of {} {}, {} {}; current 0.4.0 {}); rebuild it",
            both[0].release(),
            both[0].digest().short(),
            both[1].release(),
            both[1].digest().short(),
            reshaped.digest().short(),
        )
    );
}
