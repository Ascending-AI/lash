//! The `TS_TYPED_*` rows of the deviation register (`deviations.md`):
//! every place where trusting a type makes the dialect stricter than
//! JavaScript.
//!
//! Each row is one law. Its program declares a type, hands the operation a
//! value of another type through `any`, and must end in the row's typed
//! error; the same program with the annotations written `any` must give
//! the answer JavaScript gives.

use lash_kernel_doc::{Datum, Float};

use super::machine::{Ended, end};

struct Row {
    code: &'static str,
    /// The kind of the error lash raises.
    raises: &'static str,
    /// A program whose annotations are wrong at run time, with them as
    /// declared or written `any`.
    program: fn(declared: bool) -> String,
    /// What JavaScript, and so the `any` program, gives.
    answer: fn() -> Datum,
}

fn number(value: f64) -> Datum {
    Datum::Float(Float::new(value))
}

fn text(value: &str) -> Datum {
    Datum::Text(value.to_string())
}

/// `function f(params) { body } await control.finish(f(args));`, with each parameter
/// declared as written or as `any`.
fn program(declared: bool, params: &[(&str, &str)], body: &str, args: &str) -> String {
    let params: Vec<String> = params
        .iter()
        .map(|(name, ty)| format!("{name}: {}", if declared { ty } else { "any" }))
        .collect();
    format!(
        "function f({}) {{ {body} }} await control.finish(f({args}));",
        params.join(", ")
    )
}

const NUMBERS: &[(&str, &str)] = &[("a", "number"), ("b", "number")];
const ELEMENT: &[(&str, &str)] = &[("xs", "number[]"), ("i", "number")];

const STRINGS: &[(&str, &str)] = &[("a", "string"), ("b", "string")];

const ROWS: &[Row] = &[
    Row {
        code: "TS_TYPED_NUMBER_ADD",
        raises: "type_error",
        program: |declared| program(declared, NUMBERS, "return a + b;", "'1' as any, 2"),
        answer: || text("12"),
    },
    Row {
        code: "TS_TYPED_NUMBER_ARITHMETIC",
        raises: "type_error",
        program: |declared| program(declared, NUMBERS, "return a - b;", "'5' as any, 2"),
        answer: || number(3.0),
    },
    Row {
        code: "TS_TYPED_NUMBER_COMPARE",
        raises: "type_error",
        program: |declared| program(declared, NUMBERS, "return a < b;", "'10' as any, 9"),
        answer: || Datum::Bool(false),
    },
    Row {
        code: "TS_TYPED_NUMBER_EQUAL",
        raises: "type_error",
        program: |declared| program(declared, NUMBERS, "return a === b;", "'1' as any, 1"),
        answer: || Datum::Bool(false),
    },
    Row {
        code: "TS_TYPED_NUMBER_NEGATE",
        raises: "type_error",
        program: |declared| program(declared, &[("a", "number")], "return -a;", "true as any"),
        answer: || number(-1.0),
    },
    Row {
        code: "TS_TYPED_NUMBER_UPDATE",
        raises: "type_error",
        program: |declared| {
            program(
                declared,
                &[("a", "number")],
                "a++; return a;",
                "true as any",
            )
        },
        answer: || number(2.0),
    },
    Row {
        code: "TS_TYPED_NUMBER_METHOD",
        raises: "type_error",
        program: |declared| {
            program(
                declared,
                &[("n", "number")],
                "return n.toString();",
                "'7' as any",
            )
        },
        answer: || text("7"),
    },
    Row {
        code: "TS_TYPED_STRING_CONCAT",
        raises: "type_error",
        program: |declared| program(declared, STRINGS, "return a + b;", "true as any, 'x'"),
        answer: || text("truex"),
    },
    Row {
        code: "TS_TYPED_STRING_TEMPLATE",
        raises: "type_error",
        program: |declared| {
            program(
                declared,
                &[("a", "string")],
                "return `n=${a}`;",
                "true as any",
            )
        },
        answer: || text("n=true"),
    },
    Row {
        code: "TS_TYPED_STRING_METHOD",
        raises: "type_error",
        program: |declared| {
            program(
                declared,
                &[("s", "string")],
                "return s.indexOf('b');",
                "['a', 'b'] as any",
            )
        },
        answer: || number(1.0),
    },
    Row {
        code: "TS_TYPED_BOOLEAN_NOT",
        raises: "type_error",
        program: |declared| program(declared, &[("a", "boolean")], "return !a;", "0 as any"),
        answer: || Datum::Bool(true),
    },
    Row {
        code: "TS_TYPED_BOOLEAN_CONDITION",
        raises: "type_error",
        program: |declared| {
            program(
                declared,
                &[("a", "boolean")],
                "if (a) { return 1; } return 2;",
                "'' as any",
            )
        },
        answer: || number(2.0),
    },
    Row {
        code: "TS_TYPED_BOOLEAN_METHOD",
        raises: "type_error",
        program: |declared| {
            program(
                declared,
                &[("b", "boolean")],
                "return b.toString();",
                "1 as any",
            )
        },
        answer: || text("1"),
    },
    Row {
        code: "TS_TYPED_NUMERIC_KEY",
        raises: "function_argument",
        program: |declared| {
            program(
                declared,
                &[("key", "number")],
                "const value = {now: 7}; return value[key];",
                "'now' as any",
            )
        },
        answer: || number(7.0),
    },
    Row {
        code: "TS_TYPED_ARRAY_ELEMENT",
        raises: "type_error",
        program: |declared| program(declared, ELEMENT, "return xs[i];", "7 as any, 0"),
        answer: || Datum::Absent,
    },
    Row {
        code: "TS_TYPED_ARRAY_INDEX_RANGE",
        raises: "index_out_of_range",
        program: |declared| program(declared, ELEMENT, "return xs[i];", "[7], 5"),
        answer: || Datum::Absent,
    },
    Row {
        code: "TS_TYPED_ARRAY_LENGTH",
        raises: "type_error",
        program: |declared| {
            program(
                declared,
                &[("xs", "number[]")],
                "return xs.length;",
                "7 as any",
            )
        },
        answer: || Datum::Absent,
    },
    Row {
        code: "TS_TYPED_ARRAY_METHOD",
        raises: "type_error",
        program: |declared| {
            program(
                declared,
                &[("xs", "number[]")],
                "return xs.push(3);",
                "({ push: (v: any) => v + 1 }) as any",
            )
        },
        answer: || number(4.0),
    },
];

/// The law of one row: the declared program raises the row's typed error,
/// and the `any` program gives JavaScript's answer.
fn holds(code: &str) {
    let Some(row) = ROWS.iter().find(|row| row.code == code) else {
        panic!("no row `{code}`");
    };
    let declared = (row.program)(true);
    assert_eq!(
        end(&declared),
        Ended::Raised(row.raises.to_string()),
        "{declared}"
    );
    let untyped = (row.program)(false);
    assert_eq!(end(&untyped), Ended::Finished((row.answer)()), "{untyped}");
}

macro_rules! rows {
    ($($law:ident => $code:literal,)*) => {
        $(
            #[test]
            fn $law() {
                holds($code);
            }
        )*
    };
}

rows! {
    number_add_raises_on_a_mistyped_operand => "TS_TYPED_NUMBER_ADD",
    number_arithmetic_raises_on_a_mistyped_operand => "TS_TYPED_NUMBER_ARITHMETIC",
    number_compare_raises_on_a_mistyped_operand => "TS_TYPED_NUMBER_COMPARE",
    number_equal_raises_on_a_mistyped_operand => "TS_TYPED_NUMBER_EQUAL",
    number_negate_raises_on_a_mistyped_operand => "TS_TYPED_NUMBER_NEGATE",
    number_update_raises_on_a_mistyped_operand => "TS_TYPED_NUMBER_UPDATE",
    number_method_raises_on_a_value_that_is_not_a_number => "TS_TYPED_NUMBER_METHOD",
    string_concat_raises_on_a_mistyped_operand => "TS_TYPED_STRING_CONCAT",
    string_template_raises_on_a_mistyped_value => "TS_TYPED_STRING_TEMPLATE",
    string_method_raises_on_a_value_that_is_not_a_string => "TS_TYPED_STRING_METHOD",
    boolean_not_raises_on_a_mistyped_operand => "TS_TYPED_BOOLEAN_NOT",
    boolean_condition_raises_on_a_mistyped_test => "TS_TYPED_BOOLEAN_CONDITION",
    boolean_method_raises_on_a_value_that_is_not_a_boolean => "TS_TYPED_BOOLEAN_METHOD",
    numeric_key_raises_on_a_value_that_is_not_a_number => "TS_TYPED_NUMERIC_KEY",
    array_element_raises_on_a_value_that_is_not_an_array => "TS_TYPED_ARRAY_ELEMENT",
    array_index_out_of_range_raises => "TS_TYPED_ARRAY_INDEX_RANGE",
    array_length_raises_on_a_value_that_is_not_an_array => "TS_TYPED_ARRAY_LENGTH",
    array_method_raises_on_a_value_that_is_not_an_array => "TS_TYPED_ARRAY_METHOD",
}
