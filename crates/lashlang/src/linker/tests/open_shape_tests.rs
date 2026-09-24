//! The closed-shape field guard and what opens a shape (FIG-3626).

use super::*;

fn link(expressions: Vec<Expr>) -> Result<LinkedModule, LinkError> {
    LinkedModule::link(builders::program(expressions), full_host_environment())
}

fn record_ab() -> Expr {
    builders::record(vec![("a", builders::num(1.0)), ("b", builders::num(2.0))])
}

fn missing(error: Result<LinkedModule, LinkError>) -> (String, Vec<String>) {
    match error {
        Err(LinkError::UnknownObjectField { field, known, .. }) => (field, known),
        other => panic!("expected a missing-field refusal, got {other:?}"),
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

#[test]
fn the_refusal_names_the_field_and_the_shapes_fields() {
    // value = { a: 1, b: 2 }; finish value.c
    let refused = link(vec![
        builders::assign("value", record_ab()),
        builders::finish(builders::field(builders::var("value"), "c")),
    ]);
    let Err(error) = &refused else {
        panic!("a closed literal's missing field links");
    };
    assert_eq!(
        error.to_string(),
        "object has no field `c`; its fields are `a`, `b`"
    );
    assert_eq!(missing(refused), ("c".to_string(), names(&["a", "b"])));

    // value = {}; value.c = 1
    let empty = link(vec![
        builders::assign("value", builders::record(Vec::new())),
        builders::assign_path("value", vec![builders::field_step("c")], builders::num(1.0)),
    ]);
    let Err(error) = &empty else {
        panic!("a write of a field an empty literal lacks links");
    };
    assert_eq!(
        error.to_string(),
        "object has no field `c`; it has no fields"
    );
}

#[test]
fn a_computed_key_write_opens_the_shape_everywhere() {
    // value = { a: 1, b: 2 }; before = value.z; value[key] = 3; finish [before, value.z]
    link(vec![
        builders::assign("value", record_ab()),
        builders::assign("key", builders::string("z")),
        builders::assign("before", builders::field(builders::var("value"), "z")),
        builders::assign_path(
            "value",
            vec![builders::index_step(builders::var("key"))],
            builders::num(3.0),
        ),
        builders::finish(builders::list(vec![
            builders::var("before"),
            builders::field(builders::var("value"), "z"),
        ])),
    ])
    .expect("a shape a computed key writes is open before the write too");
}

#[test]
fn an_escape_opens_the_shape_and_a_read_that_keeps_no_reference_does_not() {
    // value = { a: 1, b: 2 }; grow = fn(x) { x }; grow(value); finish value.z
    link(vec![
        builders::assign("value", record_ab()),
        builders::assign(
            "grow",
            builders::closure(None, &["x"], &[], builders::var("x")),
        ),
        builders::call(builders::var("grow"), vec![builders::var("value")]),
        builders::finish(builders::field(builders::var("value"), "z")),
    ])
    .expect("an object passed to a function is open");

    // value = { a: 1, b: 2 }; held = [value]; finish value.z
    link(vec![
        builders::assign("value", record_ab()),
        builders::assign("held", builders::list(vec![builders::var("value")])),
        builders::finish(builders::field(builders::var("value"), "z")),
    ])
    .expect("an object stored in a container is open");

    // value = { a: 1, b: 2 }; keys = Object.keys(value); finish value.z
    let keys = link(vec![
        builders::assign("value", record_ab()),
        builders::assign(
            "keys",
            builders::builtin(
                "__typescript_stdlib",
                vec![builders::string("Object.keys"), builders::var("value")],
            ),
        ),
        builders::finish(builders::field(builders::var("value"), "z")),
    ]);
    assert_eq!(missing(keys), ("z".to_string(), names(&["a", "b"])));

    // value = { a: 1, b: 2 }; slot = globalThis.value; finish value.z
    link(vec![
        builders::assign("value", record_ab()),
        builders::assign(
            "slot",
            builders::builtin("__typescript_global_get", vec![builders::string("value")]),
        ),
        builders::finish(builders::field(builders::var("value"), "z")),
    ])
    .expect("an object the session slot shares is open");
}

#[test]
fn an_alias_is_guarded_as_its_place_and_opens_it() {
    // value = { a: 1, b: 2 }; alias = value; finish alias.z
    let alias = link(vec![
        builders::assign("value", record_ab()),
        builders::assign("alias", builders::var("value")),
        builders::finish(builders::field(builders::var("alias"), "z")),
    ]);
    assert_eq!(missing(alias), ("z".to_string(), names(&["a", "b"])));

    // value = { a: 1, b: 2 }; alias = value; alias[key] = 3; finish value.z
    link(vec![
        builders::assign("value", record_ab()),
        builders::assign("alias", builders::var("value")),
        builders::assign_path(
            "alias",
            vec![builders::index_step(builders::string("z"))],
            builders::num(3.0),
        ),
        builders::finish(builders::field(builders::var("value"), "z")),
    ])
    .expect("a computed-key write through an alias opens the aliased object");
}

#[test]
fn an_escaping_field_opens_its_object_and_not_its_holder() {
    // value = { a: 1, inner: { b: 2 } }; held = [value.inner]
    let setup = || {
        vec![
            builders::assign(
                "value",
                builders::record(vec![
                    ("a", builders::num(1.0)),
                    ("inner", builders::record(vec![("b", builders::num(2.0))])),
                ]),
            ),
            builders::assign(
                "held",
                builders::list(vec![builders::field(builders::var("value"), "inner")]),
            ),
        ]
    };
    let mut inner = setup();
    inner.push(builders::finish(builders::field(
        builders::field(builders::var("value"), "inner"),
        "z",
    )));
    link(inner).expect("the escaped field's object is open");

    let mut holder = setup();
    holder.push(builders::finish(builders::field(
        builders::var("value"),
        "z",
    )));
    assert_eq!(
        missing(link(holder)),
        ("z".to_string(), names(&["a", "inner"]))
    );

    // An element read stands for every field: held = [value[key]] opens what
    // the fields hold, not the object holding them.
    let element = link(vec![
        builders::assign(
            "value",
            builders::record(vec![(
                "inner",
                builders::record(vec![("b", builders::num(2.0))]),
            )]),
        ),
        builders::assign(
            "held",
            builders::list(vec![builders::index(
                builders::var("value"),
                builders::string("inner"),
            )]),
        ),
        builders::assign(
            "deep",
            builders::field(builders::field(builders::var("value"), "inner"), "z"),
        ),
        builders::finish(builders::field(builders::var("value"), "z")),
    ]);
    assert_eq!(missing(element), ("z".to_string(), names(&["inner"])));
}

#[test]
fn an_alias_cycle_settles() {
    // value = { a: { b: 1 } }; alias = value.a; value = alias; alias[key] = 2;
    // finish value.z
    link(vec![
        builders::assign(
            "value",
            builders::record(vec![(
                "a",
                builders::record(vec![("b", builders::num(1.0))]),
            )]),
        ),
        builders::assign("alias", builders::field(builders::var("value"), "a")),
        builders::assign("value", builders::var("alias")),
        builders::assign_path(
            "alias",
            vec![builders::index_step(builders::string("z"))],
            builders::num(2.0),
        ),
        builders::finish(builders::field(builders::var("value"), "z")),
    ])
    .expect("an alias cycle opens what it reaches and terminates");
}
