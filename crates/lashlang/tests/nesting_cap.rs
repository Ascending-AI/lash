//! The AST nesting cap must never be tighter than the parser's.
//!
//! `LinkedModule::link` is the shared entry for parsed *and* AST-built
//! programs, so a single cap governs both. The parser bounds syntactic depth,
//! but a syntactic level does not cost one AST level: block-bodied constructs
//! (`if`, `while`, `for`) each build an `Expr::Block` inside them and cost two.
//! The invariant that keeps the cap honest is therefore not an arithmetic
//! relation between two constants — it is this test: every program the parser
//! accepts must pass `check_ast_nesting_depth` and must link.

use lashlang::{
    LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, LinkedModule,
    check_ast_nesting_depth, parse,
};

/// The parsed shapes a program can nest, chosen to span the per-level AST cost
/// range: the block-bodied constructs are the expensive end, the literal and
/// operator shapes the cheap end.
fn parsed_shape_family() -> Vec<(&'static str, fn(usize) -> String)> {
    fn nest(depth: usize, wrap: impl Fn(usize, String) -> String, leaf: &str) -> String {
        let mut source = String::from(leaf);
        for level in 0..depth {
            source = wrap(level, source);
        }
        source
    }
    vec![
        ("if", |depth| {
            nest(
                depth,
                |_, s| format!("if true {{ {s} }} else {{ finish 0 }}"),
                "finish 1",
            )
        }),
        ("while", |depth| {
            format!(
                "{}\nfinish 1",
                nest(depth, |_, s| format!("while false {{ {s} }}"), "x = 1")
            )
        }),
        ("for", |depth| {
            format!(
                "{}\nfinish 1",
                nest(
                    depth,
                    |level, s| format!("for item{level} in [1] {{ {s} }}"),
                    "x = 1"
                )
            )
        }),
        ("record", |depth| {
            format!(
                "finish {}",
                nest(depth, |_, s| format!("{{ next: {s} }}"), "0")
            )
        }),
        ("list", |depth| {
            format!("finish {}", nest(depth, |_, s| format!("[{s}]"), "0"))
        }),
        ("paren", |depth| {
            format!("finish {}", nest(depth, |_, s| format!("({s})"), "0"))
        }),
        ("unary", |depth| {
            format!("finish {}", nest(depth, |_, s| format!("-({s})"), "0"))
        }),
        ("binary", |depth| {
            format!("finish {}", nest(depth, |_, s| format!("({s} + 1)"), "0"))
        }),
        ("comprehension", |depth| {
            format!(
                "finish {}",
                nest(
                    depth,
                    |level, s| format!("[n{level} for n{level} in [{s}]]"),
                    "0"
                )
            )
        }),
        ("call", |depth| {
            format!("finish {}", nest(depth, |_, s| format!("len([{s}])"), "0"))
        }),
    ]
}

fn environment() -> LashlangHostEnvironment {
    LashlangHostEnvironment::new(LashlangHostCatalog::new(), LashlangAbilities::all())
}

/// Walks every shape up to the depth the parser refuses, and requires each
/// accepted program to survive both the depth check and the linker. A cap that
/// is too tight fails here rather than in a downstream embedder.
#[test]
fn every_parsed_shape_the_parser_accepts_stays_inside_the_ast_cap() {
    let mut summary = Vec::new();
    for (name, build) in parsed_shape_family() {
        let mut deepest_accepted = 0usize;
        for depth in 1..=128usize {
            let source = build(depth);
            let Ok(program) = parse(&source) else {
                break;
            };
            deepest_accepted = depth;
            check_ast_nesting_depth(&program).unwrap_or_else(|error| {
                panic!("shape `{name}` at parser-accepted depth {depth}: {error}")
            });
            LinkedModule::link(program, environment()).unwrap_or_else(|error| {
                panic!("shape `{name}` at parser-accepted depth {depth} must link: {error}")
            });
        }
        assert!(
            deepest_accepted > 0,
            "shape `{name}` must parse at depth 1; check the generator"
        );
        summary.push((name, deepest_accepted));
    }
    println!("parser-accepted depth per shape: {summary:?}");
}

/// The margin, stated as a number so a shape family that grows more expensive
/// is visible rather than merely tolerated: the deepest tree any parsed program
/// can build must stay inside the AST cap.
#[test]
fn the_worst_parsed_shape_stays_inside_the_ast_cap() {
    let mut worst = (0usize, "none");
    for (name, build) in parsed_shape_family() {
        for depth in 1..=128usize {
            let Ok(program) = parse(&build(depth)) else {
                break;
            };
            let depth = ast_nesting_depth(&program);
            if depth > worst.0 {
                worst = (depth, name);
            }
        }
    }
    println!(
        "deepest parsed AST tree: {} levels (shape `{}`), cap {}",
        worst.0,
        worst.1,
        lashlang::MAX_AST_NESTING_DEPTH
    );
    assert!(
        worst.0 <= lashlang::MAX_AST_NESTING_DEPTH,
        "the parser admits a {}-level tree (shape `{}`) but the AST cap is {}",
        worst.0,
        worst.1,
        lashlang::MAX_AST_NESTING_DEPTH
    );
}

/// Mirrors `check_ast_nesting_depth`'s walk so the assertion above reports the
/// number, not just pass or fail.
fn ast_nesting_depth(program: &lashlang::Program) -> usize {
    let mut pending: Vec<(&lashlang::Expr, usize)> = vec![(&program.main, 1)];
    for declaration in &program.declarations {
        if let lashlang::Declaration::Process(process) = declaration {
            pending.push((&process.body, 1));
        }
    }
    let mut deepest = 0;
    while let Some((expr, depth)) = pending.pop() {
        deepest = deepest.max(depth);
        for child in expr.children() {
            pending.push((child, depth + 1));
        }
    }
    deepest
}
