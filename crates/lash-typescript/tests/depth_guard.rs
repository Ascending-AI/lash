use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

const DOCUMENTED_SOURCE_NESTING_LIMIT: usize = 28;
const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected ability in depth test")),
        }
    }
}

fn stack_budget_source(blocks: usize, parens: usize) -> String {
    format!(
        "{}finish({}1{});{}",
        "{".repeat(blocks),
        "(".repeat(parens),
        ")".repeat(parens),
        "}".repeat(blocks),
    )
}

fn delimiter_free_source(shape: &str, depth: usize) -> String {
    match shape {
        "not" => format!("finish({}1);", "!".repeat(depth)),
        "minus" => format!("finish({}1);", "- ".repeat(depth)),
        "typeof" => format!("finish({}1);", "typeof ".repeat(depth)),
        "ternary" => format!("finish({}1);", "1?1:".repeat(depth)),
        "binary" => format!("finish(1{});", "+1".repeat(depth)),
        _ => panic!("unknown delimiter-free nesting shape: {shape}"),
    }
}

fn mixed_delimiter_source(braces: usize, brackets: usize) -> String {
    format!(
        "{}finish({}1{});{}",
        "{".repeat(braces),
        "[".repeat(brackets),
        "]".repeat(brackets),
        "}".repeat(braces),
    )
}

#[test]
fn ten_thousand_nested_parens_return_a_named_diagnostic_without_aborting() {
    const CHILD_ENV: &str = "LASH_TS_DEPTH_GUARD_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        let source = format!("finish({}1{});", "(".repeat(10_000), ")".repeat(10_000));
        let error = lash_typescript::parse(&source).expect_err("nesting must be rejected");
        assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
        return;
    }

    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "ten_thousand_nested_parens_return_a_named_diagnostic_without_aborting",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .status()
        .expect("depth child starts");
    assert!(
        status.success(),
        "depth child did not fail closed: {status}"
    );
}

#[test]
fn delimiter_free_nesting_returns_a_named_diagnostic_without_aborting() {
    const CHILD_ENV: &str = "LASH_TS_DELIMITER_FREE_DEPTH_CHILD";
    const SHAPES: [&str; 5] = ["not", "minus", "typeof", "ternary", "binary"];
    if let Some(shape) = std::env::var_os(CHILD_ENV) {
        let shape = shape.to_string_lossy();
        let source = delimiter_free_source(&shape, 4_000);
        let error = lash_typescript::parse(&source).expect_err("nesting must be rejected");
        assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
        return;
    }

    for shape in SHAPES {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "delimiter_free_nesting_returns_a_named_diagnostic_without_aborting",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD_ENV, shape)
            .status()
            .expect("depth child starts");
        assert!(
            status.success(),
            "{shape} depth child did not fail closed: {status}"
        );
    }
}

#[test]
fn mixed_delimiters_share_one_source_nesting_budget() {
    // The surrounding `finish(` call consumes one level of the total.
    lash_typescript::parse(&mixed_delimiter_source(13, 14))
        .expect("28 total delimiter levels should parse");
    let error = lash_typescript::parse(&mixed_delimiter_source(14, 14))
        .expect_err("29 total delimiter levels must reject");
    assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
}

#[test]
fn sequential_statement_forms_do_not_accumulate_source_nesting() {
    // Statement keywords open a recursive form that closes with the statement.
    // A flat sequence of them is one level deep however long the sequence is.
    for source in [
        "if (1) { const a = 1; } ".repeat(200),
        "while (0) { const a = 1; } ".repeat(200),
        "if (1) { const a = 1; } else { const b = 2; } ".repeat(200),
        "if (1) { if (0) { const a = 1; } } ".repeat(200),
        (0..200)
            .map(|index| {
                format!("function f{index}(): number {{ if (1) {{ return 1; }} return 0; }} ")
            })
            .collect::<String>(),
    ] {
        lash_typescript::parse(&source).unwrap_or_else(|error| {
            panic!("flat statement sequences parse: {}", error.code.as_str())
        });
    }
    // Statement forms outside the accepted surface still reject on their own
    // terms rather than on an accumulated nesting budget.
    let error = lash_typescript::parse(&"do { const a = 1; } while (0); ".repeat(200))
        .expect_err("do/while is outside the surface");
    assert_eq!(error.code.as_str(), "TS_DO_WHILE_UNSUPPORTED");
    let error = lash_typescript::parse(&format!("{}finish(1);", "if (1) ".repeat(200)))
        .expect_err("brace-free statement nesting must still reject");
    assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
}

#[test]
fn semicolon_free_statement_sequences_do_not_accumulate_source_nesting() {
    // ASI terminates these statements; none of them ends in `;`, `,` or a
    // statement-closing brace, and none of them is nested at all.
    let declarations = (0..200)
        .map(|index| format!("const v{index} = {index}\n"))
        .collect::<String>();
    lash_typescript::parse(&format!("{declarations}finish(`${{v199}}`)\n")).unwrap_or_else(
        |error| panic!("semicolon-free declarations parse: {}", error.code.as_str()),
    );
    for source in [
        (0..200)
            .map(|index| format!("const a{index} = [{index}].length\n"))
            .collect::<String>(),
        (0..200)
            .map(|index| format!("const b{index} = `x`.length + {index}\n"))
            .collect::<String>(),
        (0..200)
            .map(|index| format!("if (1) {{ const c{index} = 1 }}\n"))
            .collect::<String>(),
        (0..200)
            .map(|index| format!("const d{index} = {index}\n// a comment\n"))
            .collect::<String>(),
    ] {
        lash_typescript::parse(&source).unwrap_or_else(|error| {
            panic!(
                "semicolon-free statement sequences parse: {}",
                error.code.as_str()
            )
        });
    }
    // The verifier's legal-direction corpus: ordinary semicolon-free
    // TypeScript, including the shapes that carry a newline past a comment or
    // across a construct.
    let trailing_comments = (0..120)
        .map(|index| format!("const t{index} = {index} // note\n"))
        .collect::<String>();
    let block_comments = (0..120)
        .map(|index| format!("const u{index} = {index} /* note */\n"))
        .collect::<String>();
    let crlf = (0..120)
        .map(|index| format!("const w{index} = {index}\r\n"))
        .collect::<String>();
    let calls = format!(
        "const f = (n: number): number => n\n{}",
        (0..120)
            .map(|index| format!("f({index})\n"))
            .collect::<String>()
    );
    let arrows = (0..60)
        .map(|index| format!("const g{index} = (n: number): number => n + {index}\n"))
        .collect::<String>();
    let newline_arrow_bodies = (0..60)
        .map(|index| format!("const h{index} = (n: number): number =>\n  n + {index}\n"))
        .collect::<String>();
    let multiline_templates = (0..60)
        .map(|index| format!("const m{index} = `line {index}\nnext line`\n"))
        .collect::<String>();
    let if_blocks = (0..60)
        .map(|index| format!("if (1) {{ const p{index} = {index} }}\n"))
        .collect::<String>();
    let mixed = (0..60)
        .map(|index| {
            format!("const q{index} = 1 > 0 ? {index} : 0\nwhile (0) {{ const r{index} = 1 }}\n")
        })
        .collect::<String>();
    for (name, source) in [
        ("trailing line comments", trailing_comments),
        ("trailing block comments", block_comments),
        ("CRLF line endings", crlf),
        ("semicolon-free calls", calls),
        ("semicolon-free arrows", arrows),
        ("newline arrow bodies", newline_arrow_bodies),
        ("multiline templates", multiline_templates),
        ("semicolon-free if blocks", if_blocks),
        ("mixed semicolon-free program", mixed),
    ] {
        lash_typescript::parse(&source)
            .unwrap_or_else(|error| panic!("{name} must parse: {}", error.code.as_str()));
    }
    // A newline that does not end a statement is not a release point: the
    // continuation still counts against the same budget.
    let error = lash_typescript::parse(&format!("const x = 1\n{}1\n", "+ 1\n".repeat(200)))
        .expect_err("a continued expression must still accumulate");
    assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
    let error = lash_typescript::parse(&format!(
        "const y = {{ a: 1 }}\ny\n{}\n",
        ".a\n".repeat(200)
    ))
    .expect_err("a newline-separated member chain must still accumulate");
    assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
}

#[test]
fn documented_source_nesting_limit_fits_the_two_mebibyte_stack_budget() {
    std::thread::Builder::new()
        .name("typescript-source-nesting-budget".to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(|| {
            let parens = DOCUMENTED_SOURCE_NESTING_LIMIT / 2;
            let blocks = DOCUMENTED_SOURCE_NESTING_LIMIT - parens - 1;
            // The blocks, grouping parentheses, and `finish(` call consume the
            // complete shared budget.
            let program = lash_typescript::compile(&stack_budget_source(blocks, parens))
                .expect("documented source nesting limit compiles");
            let outcome =
                futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
                    .expect("documented source nesting limit executes");
            assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(1.0)));

            let error = lash_typescript::parse(&stack_budget_source(blocks + 1, parens))
                .expect_err("first over-limit source must be rejected");
            assert_eq!(error.code.as_str(), "TS_SOURCE_NESTING_LIMIT");
        })
        .expect("stack-budget thread starts")
        .join()
        .expect("stack-budget thread does not abort or panic");
}

#[test]
fn no_accepted_grammar_shape_leaks_the_shared_ast_diagnostic() {
    // Every rejection carries a stable TS_* code of this dialect's own. The
    // shared AST's generic limit must never be the thing that fires: the source
    // budget has to bind first for every shape whose lowering is deeper than
    // its source nesting.
    type Shape = (&'static str, Box<dyn Fn(usize) -> String>);
    let shapes: Vec<Shape> = vec![
        (
            "template holes",
            Box::new(|count| format!("const a = 1; finish(`{}`);", "${a}".repeat(count))),
        ),
        (
            "string concatenation",
            Box::new(|count| {
                format!(
                    "finish('p0'{});",
                    (1..count).map(|i| format!(" + 'p{i}'")).collect::<String>()
                )
            }),
        ),
        (
            "nested arrays",
            Box::new(|count| {
                format!(
                    "finish(`${{{}1{}}}`);",
                    "[".repeat(count),
                    "]".repeat(count)
                )
            }),
        ),
        (
            "nested calls",
            Box::new(|count| {
                format!(
                    "const f = (n: number): number => n; finish(`${{{}1{}}}`);",
                    "f(".repeat(count),
                    ")".repeat(count)
                )
            }),
        ),
        (
            "member chain",
            Box::new(|count| {
                format!(
                    "const o = {{ a: 1 }}; finish(`${{o{}}}`);",
                    ".a".repeat(count)
                )
            }),
        ),
        (
            "prefix operators",
            Box::new(|count| format!("finish(`${{{}1}}`);", "!".repeat(count))),
        ),
        (
            "ternary chain",
            Box::new(|count| format!("finish(`${{{}1}}`);", "1?1:".repeat(count))),
        ),
        (
            "nested objects",
            Box::new(|count| {
                format!(
                    "finish(`${{{}1{}}}`);",
                    "{ a: ".repeat(count),
                    " }".repeat(count)
                )
            }),
        ),
    ];
    for (name, build) in shapes {
        for count in 1..=80 {
            let source = build(count);
            match lash_typescript::compile(&source) {
                Ok(_) => {}
                Err(error) => {
                    assert_ne!(
                        error.code.as_str(),
                        "TS_INVALID_SHARED_AST",
                        "{name} at {count} leaked the shared AST limit: {error}"
                    );
                }
            }
        }
    }
}

/// Every recursive production of the grammar **SWC parses**, one repeatable
/// unit each.
///
/// The preflight runs before SWC, and SWC parses the whole TypeScript grammar —
/// not the subset this dialect accepts. A production the dialect rejects later
/// still recurses in the parser the guard exists to protect, so the argument in
/// `src/adapter/nesting.rs` is stated against SWC's grammar and this table
/// follows it. Each unit is exercised twice: repeated on one line, and repeated
/// one per line, because a unit can be charged on one line and released by
/// automatic semicolon insertion across lines.
const RECURSIVE_FAMILIES: &[(&str, &str, &str)] = &[
    // prefix — punctuation operators
    ("prefix-not", "!", "1;"),
    ("prefix-tilde", "~", "1;"),
    ("prefix-minus", "- ", "1;"),
    ("prefix-plus", "+ ", "1;"),
    ("prefix-increment", "++", "a;"),
    ("prefix-spread", "...", "1;"),
    // prefix — keyword operators, value and type position
    ("prefix-typeof", "typeof ", "1;"),
    ("prefix-void", "void ", "1;"),
    ("prefix-delete", "delete ", "1;"),
    ("prefix-new", "new ", "1;"),
    ("prefix-await", "await ", "1;"),
    ("prefix-yield", "yield ", "1;"),
    ("prefix-keyof", "keyof ", "1;"),
    ("prefix-readonly", "readonly ", "1;"),
    ("prefix-infer", "infer ", "1;"),
    ("prefix-unique", "unique ", "1;"),
    // infix
    ("infix-add", "1+", "1;"),
    ("infix-strict-equal", "1===", "1;"),
    ("infix-and", "1&&", "1;"),
    ("infix-or", "1||", "1;"),
    ("infix-nullish", "1??", "1;"),
    ("infix-ternary", "1?1:", "1;"),
    ("infix-union", "1|", "1;"),
    ("infix-intersection", "1&", "1;"),
    ("infix-less", "1<", "1;"),
    ("infix-arrow", "() =>", "1;"),
    ("infix-as", " as number", ";"),
    ("infix-satisfies", " satisfies number", ";"),
    // postfix
    ("postfix-call", "(1)", ";"),
    ("postfix-subscript", "[0]", ";"),
    ("postfix-member", ".a", ";"),
    ("postfix-optional-member", "?.a", ";"),
    ("postfix-optional-call", "?.(1)", ";"),
    ("postfix-optional-subscript", "?.[0]", ";"),
    ("postfix-non-null", "!", ";"),
    ("postfix-tagged-template", "`x`", ";"),
    ("postfix-mixed-tails", "(1)[0].a", ";"),
    // delimiter
    ("delimiter-paren", "(", "1"),
    ("delimiter-bracket", "[", "1"),
    ("delimiter-brace", "{a:", "1"),
    ("delimiter-arrow-paren", "(() => ", "1"),
    ("delimiter-template-hole", "${a}", ""),
    ("delimiter-jsx", "<a>", "1"),
    ("delimiter-type-argument", "Array<", "1"),
    // statement form
    ("statement-if-block", "if (1) {", "const q = 1;"),
    ("statement-while-block", "while (0) {", "const q = 1;"),
    ("statement-for-block", "for (;;) {", "const q = 1;"),
    ("statement-if-bare", "if (1) ", "const q = 1;"),
    ("statement-with", "with (a) ", "const q = 1;"),
    ("statement-block", "{", "const q = 1;"),
    ("statement-label", "a:", "1;"),
    ("statement-label-distinct", "l:", "1;"),
    // Contextual keywords are legal label names. They are ordinary identifiers
    // to the parser and reserved words to the ASI rules, and the label charge
    // must not be gated on the second answer.
    ("statement-label-type", "type:", "1;"),
    ("statement-label-of", "of:", "1;"),
    ("statement-label-let", "let:", "1;"),
    ("statement-label-keyof", "keyof:", "1;"),
    ("statement-label-readonly", "readonly:", "1;"),
    ("statement-label-as", "as:", "1;"),
    ("statement-label-alternating", "type:of:", "1;"),
    ("statement-label-mixed-reserved", "type:a:let:", "1;"),
    ("statement-try", "try {", "const q = 1;"),
    ("statement-switch", "switch (a) {", "const q = 1;"),
    ("statement-class", "class C {", ""),
    ("statement-function", "function f() {", "const q = 1;"),
    // mixed families
    ("mixed-prefix-in-delimiter", "(!", "1"),
    ("mixed-prefix-in-bracket", "[!", "1"),
    ("mixed-postfix-after-prefix", "!a(1)", ";"),
    ("mixed-postfix-after-keyword", "typeof a(1)", ";"),
    ("mixed-infix-postfix", "1+a[0]", ";"),
    ("mixed-alternating", "!(1)[0].a typeof ", "1;"),
    ("mixed-label-prefix", "a:!", "1;"),
    ("mixed-as-postfix", " as number as any", ";"),
    ("mixed-delimiter-statement", "if (1) { (", "1"),
    // A prefix chain broken across lines: each newline sits mid-expression, so
    // none of them ends a statement and the run must keep accumulating.
    ("mixed-newline-alternation", "!\ntypeof\n-\n", "1;"),
];

/// Units whose one-per-line form is a legal flat sequence of complete
/// statements rather than a nesting: each line ends a statement and the next
/// line opens a new one, so automatic semicolon insertion is right to release
/// the budget and the source is right to be accepted. Both axes must still
/// avoid aborting the process, which is what the child proves by exiting.
const FLAT_WHEN_SPLIT: &[&str] = &[
    "mixed-postfix-after-prefix",
    "mixed-postfix-after-keyword",
    "mixed-infix-postfix",
];

fn family_source(unit: &str, tail: &str, repeats: usize, per_line: bool) -> String {
    let unit = if per_line {
        format!("{unit}\n")
    } else {
        unit.to_string()
    };
    // Never build a source the size guard would reject before the nesting guard
    // sees it: the unit sweep is about the nesting diagnostic.
    let repeats = repeats.min((lash_typescript::MAX_SOURCE_BYTES - 64) / unit.len().max(1));
    let body = unit.repeat(repeats);
    // A leading binding keeps the postfix, member and cast families applied to a
    // real expression, which is the shape that recurses.
    if tail == ";" {
        format!("const a = 1; const x = a{body}{tail}")
    } else if unit.trim_end() == "${a}" {
        format!("const a = 1; const x = `{body}`;")
    } else {
        format!("const a = 1; const x = {body}{tail}")
    }
}

/// The standing guard on the no-abort property: every recursive production,
/// repeated far past any stack, must return the named diagnostic from a child
/// process that exits cleanly.
#[test]
fn every_recursive_production_family_rejects_without_aborting() {
    const CHILD_ENV: &str = "LASH_TS_FAMILY_CHILD";
    // Fill the accepted source bound, which is the deepest an accepted cell
    // can nest. For a two-byte unit that is over thirty thousand levels.
    const REPEATS: usize = lash_typescript::MAX_SOURCE_BYTES / 2;
    if let Some(family) = std::env::var_os(CHILD_ENV) {
        let family = family.to_string_lossy().to_string();
        let (_, unit, tail) = RECURSIVE_FAMILIES
            .iter()
            .find(|(name, _, _)| *name == family)
            .expect("known family");
        let sources = [
            ("one line", family_source(unit, tail, REPEATS, false)),
            ("one per line", family_source(unit, tail, REPEATS, true)),
        ];
        std::thread::Builder::new()
            .stack_size(STACK_BUDGET_BYTES)
            .spawn(move || {
                let flat_when_split = FLAT_WHEN_SPLIT.contains(&family.as_str());
                for (axis, source) in sources {
                    let outcome = lash_typescript::parse(&source);
                    if flat_when_split && axis == "one per line" {
                        // A flat statement sequence must not be charged as
                        // nesting; reaching here at all proves it did not abort.
                        if let Err(error) = outcome {
                            assert_ne!(
                                error.code.as_str(),
                                "TS_SOURCE_NESTING_LIMIT",
                                "{family} ({axis}) is a flat sequence, not a nesting"
                            );
                        }
                        continue;
                    }
                    let error = outcome.expect_err("a repeated recursive production must reject");
                    assert_eq!(
                        error.code.as_str(),
                        "TS_SOURCE_NESTING_LIMIT",
                        "{family} ({axis}) rejected for the wrong reason"
                    );
                }
            })
            .expect("family thread starts")
            .join()
            .expect("family thread does not abort or panic");
        return;
    }

    for (family, _, _) in RECURSIVE_FAMILIES {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "every_recursive_production_family_rejects_without_aborting",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD_ENV, family)
            .status()
            .expect("family child starts");
        assert!(status.success(), "{family} did not fail closed: {status}");
    }
}

/// The lexical-fidelity axis: the scanner has its own lexer, and it must agree
/// with SWC's about where a token ends.
///
/// A grammar family can be charged correctly and still be defeated if the
/// scanner disagrees with the parser about tokenisation — a charge gated on
/// "the previous token was an identifier" never fires if the identifier was
/// split in half. That is a different failure surface from the grammar
/// families, and neither the AST classification nor an ASCII-alphabet fuzzer
/// can see it, so it gets its own enumeration: non-ASCII identifier
/// characters, identifier escapes, numeric separators, and the Unicode line
/// terminators.
const LEXICAL_UNITS: &[&str] = &[
    "label-trailing-latin",
    "label-trailing-cjk",
    "label-trailing-tilde",
    "label-leading-latin",
    "label-escaped",
    "label-escaped-braced",
    "label-numeric-separator",
    "label-line-separator",
    "label-paragraph-separator",
    "postfix-after-unicode",
    "cast-after-unicode",
    "member-after-unicode",
    "prefix-before-unicode",
    "template-after-unicode",
];

/// Sources built to nest, one per lexical unit. Each returns the source that
/// repeats the unit `repeats` times, optionally one per line.
fn lexical_source(unit: &str, repeats: usize, per_line: bool) -> String {
    let separator = if per_line { "\n" } else { "" };
    let mut source = String::new();
    match unit {
        "label-trailing-latin" => {
            for index in 0..repeats {
                source.push_str(&format!("a{index}\u{e9}:{separator}"));
            }
            source.push_str("1;");
        }
        "label-trailing-cjk" => {
            for index in 0..repeats {
                source.push_str(&format!("a{index}\u{4e2d}:{separator}"));
            }
            source.push_str("1;");
        }
        "label-trailing-tilde" => {
            for index in 0..repeats {
                source.push_str(&format!("a{index}\u{f1}:{separator}"));
            }
            source.push_str("1;");
        }
        "label-leading-latin" => {
            for index in 0..repeats {
                source.push_str(&format!("\u{e9}{index}:{separator}"));
            }
            source.push_str("1;");
        }
        // An identifier written with an escape is the same identifier to SWC.
        "label-escaped" => {
            for index in 0..repeats {
                source.push_str(&format!("a{index}\\u00e9:{separator}"));
            }
            source.push_str("1;");
        }
        "label-escaped-braced" => {
            for index in 0..repeats {
                source.push_str(&format!("a{index}\\u{{e9}}:{separator}"));
            }
            source.push_str("1;");
        }
        "label-numeric-separator" => {
            for index in 0..repeats {
                source.push_str(&format!("a1_0{index}:{separator}"));
            }
            source.push_str("1;");
        }
        // U+2028 and U+2029 are ECMAScript line terminators.
        "label-line-separator" => {
            for index in 0..repeats {
                source.push_str(&format!("a{index}:\u{2028}"));
            }
            source.push_str("1;");
        }
        "label-paragraph-separator" => {
            for index in 0..repeats {
                source.push_str(&format!("a{index}:\u{2029}"));
            }
            source.push_str("1;");
        }
        "postfix-after-unicode" => {
            source.push_str("const f\u{e9} = (n: number): number => n; const x = f\u{e9}");
            for _ in 0..repeats {
                source.push_str(&format!("(1){separator}"));
            }
            source.push(';');
        }
        "cast-after-unicode" => {
            source.push_str("const x\u{e9} = 1");
            for _ in 0..repeats {
                source.push_str(&format!(" as number{separator}"));
            }
            source.push(';');
        }
        "member-after-unicode" => {
            source.push_str("const o\u{e9} = { a: 1 }; const x = o\u{e9}");
            for _ in 0..repeats {
                source.push_str(&format!(".a{separator}"));
            }
            source.push(';');
        }
        "prefix-before-unicode" => {
            source.push_str("const x = ");
            for _ in 0..repeats {
                source.push_str(&format!("!{separator}"));
            }
            source.push_str("a\u{e9};");
        }
        "template-after-unicode" => {
            source.push_str("const a\u{e9} = 1; const x = a\u{e9}");
            for _ in 0..repeats {
                source.push_str(&format!("`x`{separator}"));
            }
            source.push(';');
        }
        other => panic!("unknown lexical unit: {other}"),
    }
    source
}

#[test]
fn lexical_units_reject_without_aborting() {
    const CHILD_ENV: &str = "LASH_TS_LEXICAL_CHILD";
    const REPEATS: usize = 4_000;
    if let Some(unit) = std::env::var_os(CHILD_ENV) {
        let unit = unit.to_string_lossy().to_string();
        let sources = [
            ("one line", lexical_source(&unit, REPEATS, false)),
            ("one per line", lexical_source(&unit, REPEATS, true)),
        ];
        std::thread::Builder::new()
            .stack_size(STACK_BUDGET_BYTES)
            .spawn(move || {
                for (axis, source) in sources {
                    let error = lash_typescript::parse(&source)
                        .expect_err("a repeated lexical unit must reject");
                    assert_eq!(
                        error.code.as_str(),
                        "TS_SOURCE_NESTING_LIMIT",
                        "{unit} ({axis}) rejected for the wrong reason"
                    );
                }
            })
            .expect("lexical thread starts")
            .join()
            .expect("lexical thread does not abort or panic");
        return;
    }

    for unit in LEXICAL_UNITS {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "lexical_units_reject_without_aborting",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD_ENV, unit)
            .status()
            .expect("lexical child starts");
        assert!(status.success(), "{unit} did not fail closed: {status}");
    }
}

/// The same axis in the legal direction: none of these lexical forms may cause
/// a spurious rejection.
#[test]
fn lexical_forms_do_not_cause_false_rejections() {
    let unicode_declarations = (0..120)
        .map(|index| format!("const v\u{e9}{index} = {index};"))
        .collect::<String>();
    let escaped_declarations = (0..120)
        .map(|index| format!("const v\\u00e9{index} = {index};"))
        .collect::<String>();
    let numeric_separators = (0..120)
        .map(|index| format!("const n{index} = 1_000_{index:03};"))
        .collect::<String>();
    // U+2028 as the only line terminator, with no semicolons.
    let line_separated = (0..120)
        .map(|index| format!("const s{index} = {index}\u{2028}"))
        .collect::<String>();
    let paragraph_separated = (0..120)
        .map(|index| format!("const p{index} = {index}\u{2029}"))
        .collect::<String>();
    let cjk_identifiers = (0..120)
        .map(|index| format!("const \u{4e2d}{index} = {index};"))
        .collect::<String>();
    for (name, source) in [
        ("unicode identifiers", unicode_declarations),
        ("escaped identifiers", escaped_declarations),
        ("numeric separators", numeric_separators),
        ("U+2028 line separators", line_separated),
        ("U+2029 paragraph separators", paragraph_separated),
        ("CJK identifiers", cjk_identifiers),
    ] {
        lash_typescript::parse(&source)
            .unwrap_or_else(|error| panic!("{name} must parse: {}", error.code.as_str()));
    }
}
