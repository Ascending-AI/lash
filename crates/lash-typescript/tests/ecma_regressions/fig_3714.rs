//! FIG-3714: switch case-block scoping, nested-switch `break`, nested
//! `for...in`, and callback `this` match ECMA-262.

use super::*;

/// FIG-3714: the whole case block is one declarative environment
/// (ECMA-262 14.2.4 CaseBlockEvaluation): a `let` declared in one clause is
/// visible to a closure made in a sibling clause, and the outer binding of
/// the same name is unaffected. Mirrors
/// `language/statements/switch/scope-lex-close-case.js`.
#[test]
fn switch_case_clauses_share_one_lexical_environment() {
    assert_eq!(
        finished(
            "let x = 'outside';\
             var probe1, probe2;\
             switch (null) {\
               case null:\
                 let x = 'inside';\
                 probe1 = function () { return x; };\
               case null:\
                 probe2 = function () { return x; };\
             }\
             finish(probe1() + '|' + probe2() + '|' + x);"
        ),
        Value::String("inside|inside|outside".into())
    );
}

/// FIG-3714: a `default` clause's `let` lands in the same case-block
/// environment, so a closure from a clause that follows it sees the inner
/// binding. Mirrors `scope-lex-close-dflt.js`.
#[test]
fn switch_default_clause_shares_the_case_block_environment() {
    assert_eq!(
        finished(
            "let x = 'outside';\
             var probe1, probe2;\
             switch (0) {\
               default:\
                 let x = 'inside';\
                 probe1 = function () { return x; };\
               case 1:\
                 probe2 = function () { return x; };\
             }\
             finish(probe1() + '|' + probe2() + '|' + x);"
        ),
        Value::String("inside|inside|outside".into())
    );
}

/// FIG-3714: a `break` exits only its own switch — the inner switch's break
/// does not end the outer consequent — and the statements after a `break`
/// inside the same consequent do not run. Mirrors
/// `language/statements/switch/S12.11_A4_T1.js`.
#[test]
fn switch_break_exits_only_its_own_switch() {
    assert_eq!(
        finished(
            "var run = function (value) {\
               var result = 0;\
               switch (value) {\
                 case 0:\
                   switch (value) {\
                     case 0:\
                       result += 3;\
                       break;\
                     default:\
                       result += 32;\
                       break;\
                   }\
                   result *= 2;\
                   break;\
                   result = 3;\
                 default:\
                   result += 32;\
                   break;\
               }\
               return result;\
             };\
             finish(run(0));"
        ),
        Value::Number(6.0)
    );
}

/// FIG-3714: statements after a flag-breaking `break` in the same consequent
/// are unreachable — including inside a nested block — while the following
/// clauses' fall-through still stops at the break.
#[test]
fn switch_break_makes_the_rest_of_its_consequent_unreachable() {
    assert_eq!(
        finished(
            "var acc = '';\
             switch (0) {\
               case 0:\
                 acc += 'a';\
                 { break; acc += 'b'; }\
                 acc += 'c';\
               case 1:\
                 acc += 'd';\
             }\
             finish(acc);"
        ),
        Value::String("a".into())
    );
}

/// FIG-3714: a nested `for...in` over a record of records visits every inner
/// member. Mirrors `language/statements/for-in/S12.6.4_A5.js`.
#[test]
fn nested_for_in_visits_record_of_records() {
    assert_eq!(
        finished(
            "var map = { a: { aa: 1, ab: 2 }, b: { ba: 1 } };\
             var acc = '';\
             for (var key in map) {\
               for (var inner in map[key]) {\
                 acc += '' + inner + map[key][inner];\
               }\
             }\
             finish(acc.indexOf('aa1') !== -1\
               && acc.indexOf('ab2') !== -1\
               && acc.indexOf('ba1') !== -1\
               && acc.length === 9);"
        ),
        Value::Bool(true)
    );
}

/// FIG-3714: a callback's `this` is exactly the `thisArg` it was called
/// with — object identity included, so a `flatMap` that returns `this`
/// hands back the same record. Mirrors
/// `built-ins/Array/prototype/flatMap/thisArg-argument.js`.
#[test]
fn array_callback_this_is_the_this_arg_itself() {
    assert_eq!(
        finished(
            "var a = { marker: 1 };\
             finish([1].flatMap(function () { return [this]; }, a)[0] === a);"
        ),
        Value::Bool(true)
    );
    for (source, expected) in [
        (
            "finish([1].flatMap(function () { return [this]; }, 's')[0]);",
            "s",
        ),
        (
            "finish([1].map(function () { return this; }, 's')[0]);",
            "s",
        ),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
    // A collected callback result that reaches back into the caller's frame
    // keeps its identity too: `map` returns the same element objects.
    assert_eq!(
        finished(
            "var held = { v: 1 };\
             var out = [held].map(function (x) { return this.target; }, { target: held });\
             finish(out[0] === held);"
        ),
        Value::Bool(true)
    );
}
