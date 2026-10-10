//! The `TS_TYPED_*` rows of the deviation register (`deviations.md`):
//! every place where trusting a type makes the dialect stricter than
//! JavaScript.
//!
//! Each row is one law. Its program declares a type, hands the operation a
//! value of another type through `any`, and must end in the row's typed
//! error; the same program with the annotations written `any` must give
//! the answer JavaScript gives. The register's rows are checked against
//! the rows here, so a row cannot be promised without its law or changed
//! without its text.

use lash_kernel_doc::{Datum, Float};

use super::machine::{Ended, end};

const REGISTER: &str = include_str!("../../deviations.md");

struct Row {
    code: &'static str,
    /// What ECMAScript does, as the register words it.
    ecmascript: &'static str,
    /// What the dialect does, as the register words it.
    dialect: &'static str,
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

/// `function f(params) { body } await finish(f(args));`, with each parameter
/// declared as written or as `any`.
fn program(declared: bool, params: &[(&str, &str)], body: &str, args: &str) -> String {
    let params: Vec<String> = params
        .iter()
        .map(|(name, ty)| format!("{name}: {}", if declared { ty } else { "any" }))
        .collect();
    format!(
        "function f({}) {{ {body} }} await finish(f({args}));",
        params.join(", ")
    )
}

const NUMBERS: &[(&str, &str)] = &[("a", "number"), ("b", "number")];
const ELEMENT: &[(&str, &str)] = &[("xs", "number[]"), ("i", "number")];

const STRINGS: &[(&str, &str)] = &[("a", "string"), ("b", "string")];

const ROWS: &[Row] = &[
    Row {
        code: "TS_TYPED_NUMBER_ADD",
        ecmascript: "`a + b` converts both operands, then adds or concatenates.",
        dialect: "With both operands declared `number`, a value that is not a number raises `type_error`.",
        raises: "type_error",
        program: |declared| program(declared, NUMBERS, "return a + b;", "'1' as any, 2"),
        answer: || text("12"),
    },
    Row {
        code: "TS_TYPED_NUMBER_ARITHMETIC",
        ecmascript: "`a - b`, `a * b`, `a / b` and `a % b` convert both operands to numbers.",
        dialect: "With both operands declared `number`, a value that is not a number raises `type_error`.",
        raises: "type_error",
        program: |declared| program(declared, NUMBERS, "return a - b;", "'5' as any, 2"),
        answer: || number(3.0),
    },
    Row {
        code: "TS_TYPED_NUMBER_COMPARE",
        ecmascript: "`a < b`, `a <= b`, `a > b` and `a >= b` convert both operands, then compare.",
        dialect: "With both operands declared `number`, a value that is not a number raises `type_error`.",
        raises: "type_error",
        program: |declared| program(declared, NUMBERS, "return a < b;", "'10' as any, 9"),
        answer: || Datum::Bool(false),
    },
    Row {
        code: "TS_TYPED_NUMBER_EQUAL",
        ecmascript: "`a === b`, `a !== b`, `a == b` and `a != b` compare any two values without raising.",
        dialect: "With both operands declared `number`, a value that is not a number raises `type_error`.",
        raises: "type_error",
        program: |declared| program(declared, NUMBERS, "return a === b;", "'1' as any, 1"),
        answer: || Datum::Bool(false),
    },
    Row {
        code: "TS_TYPED_NUMBER_NEGATE",
        ecmascript: "`-a` and `+a` convert the operand to a number.",
        dialect: "With the operand declared `number`, a value that is not a number raises `type_error`.",
        raises: "type_error",
        program: |declared| program(declared, &[("a", "number")], "return -a;", "true as any"),
        answer: || number(-1.0),
    },
    Row {
        code: "TS_TYPED_NUMBER_UPDATE",
        ecmascript: "`a++`, `a--`, `++a` and `--a` convert the operand to a number.",
        dialect: "With the operand declared `number`, a value that is not a number raises `type_error`.",
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
        ecmascript: "`n.m(...)` calls the method `m` of whatever `n` is.",
        dialect: "With `n` declared a `number`, a value that is not a number raises `type_error`.",
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
        ecmascript: "`a + b` with a string operand converts the other to a string.",
        dialect: "With both operands declared `string`, a value that is not a string raises `type_error`.",
        raises: "type_error",
        program: |declared| program(declared, STRINGS, "return a + b;", "true as any, 'x'"),
        answer: || text("truex"),
    },
    Row {
        code: "TS_TYPED_STRING_TEMPLATE",
        ecmascript: "`${a}` in a template converts the value to a string.",
        dialect: "With the value declared `string`, a value that is not a string raises `type_error`.",
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
        ecmascript: "`s.m(...)` calls the method `m` of whatever `s` is.",
        dialect: "With `s` declared a `string`, a value that is not a string raises `type_error`.",
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
        ecmascript: "`!a` takes the operand's truthiness.",
        dialect: "With the operand declared `boolean`, a value that is not a boolean raises `type_error`.",
        raises: "type_error",
        program: |declared| program(declared, &[("a", "boolean")], "return !a;", "0 as any"),
        answer: || Datum::Bool(true),
    },
    Row {
        code: "TS_TYPED_BOOLEAN_CONDITION",
        ecmascript: "`if (a)`, `while (a)`, `a ? b : c`, `a && b` and `a \\|\\| b` take the test's truthiness.",
        dialect: "With the test declared `boolean`, a value that is not a boolean raises `type_error`.",
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
        ecmascript: "`b.m(...)` calls the method `m` of whatever `b` is.",
        dialect: "With `b` declared a `boolean`, a value that is not a boolean raises `type_error`.",
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
        code: "TS_TYPED_ARRAY_ELEMENT",
        ecmascript: "`xs[i]` reads the property `i` of whatever `xs` is.",
        dialect: "With `xs` declared an array and `i` a `number`, a value that is not an array raises `type_error`.",
        raises: "type_error",
        program: |declared| program(declared, ELEMENT, "return xs[i];", "7 as any, 0"),
        answer: || Datum::Absent,
    },
    Row {
        code: "TS_TYPED_ARRAY_INDEX_RANGE",
        ecmascript: "`xs[i]` is `undefined` when `i` is negative, fractional or past the end.",
        dialect: "With `xs` declared an array and `i` a `number`, such an index raises `index_out_of_range`.",
        raises: "index_out_of_range",
        program: |declared| program(declared, ELEMENT, "return xs[i];", "[7], 5"),
        answer: || Datum::Absent,
    },
    Row {
        code: "TS_TYPED_ARRAY_LENGTH",
        ecmascript: "`xs.length` reads the property of whatever `xs` is.",
        dialect: "With `xs` declared an array, a value that is not an array raises `type_error`.",
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
        ecmascript: "`xs.m(...)` calls the method `m` of whatever `xs` is.",
        dialect: "With `xs` declared an array, a value that is not an array raises `type_error`.",
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
    Row {
        code: "TS_TYPED_ARRAY_ITERATION",
        ecmascript: "`for (x of xs)` iterates whatever `xs` is.",
        dialect: "With `xs` declared an array, a value that is not an array raises `type_error`.",
        raises: "type_error",
        program: |declared| {
            program(
                declared,
                &[("xs", "number[]")],
                "let n = 0; for (const x of xs) { n = n + 1; } return n;",
                "'ab' as any",
            )
        },
        answer: || number(2.0),
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

        /// Each row's code and the law the register cites for it.
        const LAWS: &[(&str, &str)] = &[
            $(($code, concat!("tests::deviations::", stringify!($law)))),*
        ];
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
    array_element_raises_on_a_value_that_is_not_an_array => "TS_TYPED_ARRAY_ELEMENT",
    array_index_out_of_range_raises => "TS_TYPED_ARRAY_INDEX_RANGE",
    array_length_raises_on_a_value_that_is_not_an_array => "TS_TYPED_ARRAY_LENGTH",
    array_method_raises_on_a_value_that_is_not_an_array => "TS_TYPED_ARRAY_METHOD",
    array_iteration_raises_on_a_value_that_is_not_an_array => "TS_TYPED_ARRAY_ITERATION",
}

/// The register's `TS_TYPED_*` rows are the rows here, in the same words,
/// and each cites its law.
#[test]
fn the_register_holds_every_typed_row_with_its_law() {
    let written: Vec<Vec<&str>> = REGISTER
        .lines()
        .filter(|line| line.starts_with("| `TS_TYPED_"))
        .map(|line| line.trim_matches('|').split(" | ").map(str::trim).collect())
        .collect();
    assert_eq!(written.len(), ROWS.len());
    for ((written, row), (code, law)) in written.iter().zip(ROWS).zip(LAWS) {
        assert_eq!(row.code, *code);
        let [code, ecmascript, dialect, _why, cited] = written[..] else {
            panic!("a row has five cells: {written:?}");
        };
        assert_eq!(code, format!("`{}`", row.code));
        assert_eq!(ecmascript, row.ecmascript);
        assert_eq!(dialect, row.dialect);
        assert!(dialect.contains(&format!("`{}`", row.raises)), "{dialect}");
        assert_eq!(cited, format!("`{law}`"));
    }
}
