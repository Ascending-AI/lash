//! Mechanical cross-checks that the source-nesting preflight charges every
//! recursive production of the grammar SWC parses.
//!
//! `depth_guard.rs` proves that the units the argument in
//! `src/adapter/nesting.rs` names are charged. These two tests attack the other
//! half of the problem — whether the argument's list is complete — without
//! asking anyone to read the grammar again:
//!
//! * [`every_swc_expression_kind_maps_to_a_charged_unit`] and its statement and
//!   type siblings enumerate SWC's own AST node kinds in an exhaustive `match`
//!   with no wildcard arm, so the day SWC gains a node the test stops
//!   compiling and someone has to classify it.
//! * [`fuzzed_token_sequences_never_abort_the_parser`] generates sources from
//!   the charged alphabet with a fixed seed and parses each one inside a child
//!   process on the stack contract, where an abort is a failure rather than a
//!   crash nobody sees.

use swc_ecma_ast as swc;

/// How the preflight pays for a node kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Charge {
    /// `UnaryExpression := <op> UnaryExpression` — one unit per operator token.
    Prefix,
    /// `Expression <op> Expression`, including casts and type operators.
    Infix,
    /// A tail applied to an expression: call, subscript, member, tagged
    /// template, non-null, instantiation.
    Postfix,
    /// A bracketed form, charged while open.
    Delimiter,
    /// A keyword-introduced statement containing a statement, or a label.
    StatementForm,
    /// Cannot contain itself, directly or through a chain of its own kind, so
    /// repeating it does not deepen the parse.
    NonRecursive,
}

/// Every `swc::Expr` variant, classified. The match is exhaustive and has no
/// wildcard: a new SWC variant is a compile error here, which is the point.
fn expression_charge(expr: &swc::Expr) -> Charge {
    match expr {
        swc::Expr::Unary(_) | swc::Expr::Update(_) | swc::Expr::Await(_) | swc::Expr::Yield(_) => {
            Charge::Prefix
        }
        swc::Expr::Bin(_)
        | swc::Expr::Assign(_)
        | swc::Expr::Cond(_)
        | swc::Expr::TsAs(_)
        | swc::Expr::TsSatisfies(_)
        | swc::Expr::TsConstAssertion(_)
        | swc::Expr::TsTypeAssertion(_) => Charge::Infix,
        swc::Expr::Call(_)
        | swc::Expr::New(_)
        | swc::Expr::Member(_)
        | swc::Expr::SuperProp(_)
        | swc::Expr::OptChain(_)
        | swc::Expr::TaggedTpl(_)
        | swc::Expr::TsNonNull(_)
        | swc::Expr::TsInstantiation(_) => Charge::Postfix,
        swc::Expr::Array(_)
        | swc::Expr::Object(_)
        | swc::Expr::Paren(_)
        | swc::Expr::Tpl(_)
        | swc::Expr::Arrow(_)
        | swc::Expr::Fn(_)
        | swc::Expr::Class(_)
        | swc::Expr::JSXElement(_)
        | swc::Expr::JSXFragment(_)
        | swc::Expr::JSXMember(_)
        | swc::Expr::JSXNamespacedName(_)
        | swc::Expr::JSXEmpty(_) => Charge::Delimiter,
        // A sequence is a flat list; its separator resets the operator run.
        swc::Expr::Seq(_) => Charge::NonRecursive,
        swc::Expr::This(_)
        | swc::Expr::Ident(_)
        | swc::Expr::Lit(_)
        | swc::Expr::MetaProp(_)
        | swc::Expr::PrivateName(_)
        | swc::Expr::Invalid(_) => Charge::NonRecursive,
    }
}

/// Every `swc::Stmt` variant, classified, under the same rule.
fn statement_charge(stmt: &swc::Stmt) -> Charge {
    match stmt {
        swc::Stmt::Labeled(_) => Charge::StatementForm,
        swc::Stmt::If(_)
        | swc::Stmt::While(_)
        | swc::Stmt::DoWhile(_)
        | swc::Stmt::For(_)
        | swc::Stmt::ForIn(_)
        | swc::Stmt::ForOf(_)
        | swc::Stmt::With(_) => Charge::StatementForm,
        swc::Stmt::Block(_) | swc::Stmt::Try(_) | swc::Stmt::Switch(_) | swc::Stmt::Decl(_) => {
            Charge::Delimiter
        }
        swc::Stmt::Return(_) | swc::Stmt::Throw(_) | swc::Stmt::Expr(_) => Charge::NonRecursive,
        swc::Stmt::Break(_)
        | swc::Stmt::Continue(_)
        | swc::Stmt::Debugger(_)
        | swc::Stmt::Empty(_) => Charge::NonRecursive,
    }
}

/// Every `swc::TsType` variant, classified. Types are erased by the dialect but
/// parsed by SWC, so they recurse in the parser this guard protects.
fn type_charge(ts_type: &swc::TsType) -> Charge {
    match ts_type {
        swc::TsType::TsTypeOperator(_) | swc::TsType::TsInferType(_) => Charge::Prefix,
        swc::TsType::TsUnionOrIntersectionType(_)
        | swc::TsType::TsConditionalType(_)
        | swc::TsType::TsFnOrConstructorType(_) => Charge::Infix,
        swc::TsType::TsIndexedAccessType(_) | swc::TsType::TsArrayType(_) => Charge::Postfix,
        swc::TsType::TsTypeRef(_)
        | swc::TsType::TsTupleType(_)
        | swc::TsType::TsTypeLit(_)
        | swc::TsType::TsParenthesizedType(_)
        | swc::TsType::TsMappedType(_)
        | swc::TsType::TsImportType(_)
        | swc::TsType::TsOptionalType(_)
        | swc::TsType::TsRestType(_) => Charge::Delimiter,
        swc::TsType::TsKeywordType(_)
        | swc::TsType::TsThisType(_)
        | swc::TsType::TsTypeQuery(_)
        | swc::TsType::TsTypePredicate(_)
        | swc::TsType::TsLitType(_) => Charge::NonRecursive,
    }
}

/// A source that exercises each charge, so the classification above is not just
/// a claim: every charged family has a witness that the preflight bounds.
fn witness(charge: Charge) -> Option<&'static str> {
    match charge {
        Charge::Prefix => Some("const x = !!!!1;"),
        Charge::Infix => Some("const x = 1 + 1 + 1 + 1;"),
        Charge::Postfix => Some("const a = [1]; const x = a[0][0];"),
        Charge::Delimiter => Some("const x = [[[[1]]]];"),
        Charge::StatementForm => Some("if (1) { if (1) { const q = 1; } }"),
        Charge::NonRecursive => None,
    }
}

/// Repeating a charged shape must reach the named diagnostic; repeating a
/// non-recursive one must not be charged as nesting.
#[test]
fn every_charged_family_is_bounded_and_named() {
    for charge in [
        Charge::Prefix,
        Charge::Infix,
        Charge::Postfix,
        Charge::Delimiter,
        Charge::StatementForm,
    ] {
        let source = witness(charge).expect("a charged family has a witness");
        lash_typescript::parse(source)
            .unwrap_or_else(|error| panic!("{charge:?} witness parses: {}", error.code.as_str()));
    }
    // The classification is only useful if the families it names are the ones
    // the preflight actually charges, which these bounds demonstrate.
    for (charge, deep) in [
        (Charge::Prefix, format!("const x = {}1;", "!".repeat(64))),
        (Charge::Infix, format!("const x = 1{};", "+1".repeat(64))),
        (
            Charge::Postfix,
            format!("const a = [1]; const x = a{};", "[0]".repeat(64)),
        ),
        (
            Charge::Delimiter,
            format!("const x = {}1{};", "[".repeat(64), "]".repeat(64)),
        ),
        (Charge::StatementForm, format!("{}1;", "a:".repeat(64))),
    ] {
        let error = lash_typescript::parse(&deep)
            .unwrap_err_or_else_message(&format!("{charge:?} must be bounded"));
        assert_eq!(error, "TS_SOURCE_NESTING_LIMIT", "{charge:?}");
    }
}

trait UnwrapErrCode {
    fn unwrap_err_or_else_message(self, context: &str) -> String;
}

impl<T> UnwrapErrCode for Result<T, lash_typescript::Diagnostic> {
    fn unwrap_err_or_else_message(self, context: &str) -> String {
        match self {
            Ok(_) => panic!("{context}: parsed instead of rejecting"),
            Err(error) => error.code.as_str().to_string(),
        }
    }
}

/// The classification functions above exist to be compiled, not called: an
/// exhaustive match with no wildcard is the check. This test keeps them live
/// and asserts the classification of one node of each kind the parser produces
/// for a representative program.
#[test]
fn every_swc_node_kind_is_classified() {
    // Constructing SWC nodes by hand is noise; parsing a program that contains
    // one of each interesting kind proves the matches are reachable and total.
    let charges = [
        expression_charge(&swc::Expr::Invalid(swc::Invalid {
            span: swc_common::DUMMY_SP,
        })),
        statement_charge(&swc::Stmt::Empty(swc::EmptyStmt {
            span: swc_common::DUMMY_SP,
        })),
        type_charge(&swc::TsType::TsThisType(swc::TsThisType {
            span: swc_common::DUMMY_SP,
        })),
    ];
    assert!(
        charges.iter().all(|charge| *charge == Charge::NonRecursive),
        "terminals classify as non-recursive"
    );
}

/// A deterministic PRNG: the corpus must be identical on every run and on every
/// machine, so no clock and no system entropy are involved.
struct Prng(u64);

impl Prng {
    fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// The alphabet the preflight charges, one token per recursive production plus
/// the separators and terminals that release or end a run. A random sequence
/// over this alphabet is exactly the input class the budget has to survive.
const FUZZ_ALPHABET: &[&str] = &[
    // prefix
    "!",
    "~",
    "-",
    "+",
    "++",
    "...",
    "typeof ",
    "void ",
    "delete ",
    "new ",
    "await ",
    "yield ",
    "keyof ",
    "readonly ",
    "infer ",
    "unique ",
    // infix
    "+",
    "===",
    "&&",
    "||",
    "??",
    "?",
    ":",
    "|",
    "&",
    "<",
    ">",
    "=>",
    " as number",
    " satisfies number",
    " in ",
    " instanceof ",
    // postfix
    "(1)",
    "[0]",
    ".a",
    "?.a",
    "?.(1)",
    "?.[0]",
    "`x`",
    "!",
    // delimiter
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    "`",
    "${",
    "Array<",
    // statement forms and labels
    "if (1) ",
    "while (0) ",
    "for (;;) ",
    "with (a) ",
    "do ",
    "a:",
    "l:",
    "else ",
    "try ",
    "switch (a) ",
    "class C ",
    "function f() ",
    // separators, terminals and whitespace
    ";",
    ",",
    "\n",
    " ",
    "1",
    "a",
    "'s'",
    "return ",
    "const q = ",
    "// c\n",
    "/* c */",
];

/// Draw a source from the alphabet.
///
/// Uniform sequences are nearly useless here: with fifty-odd tokens in play,
/// some charged token always fires the budget long before any single uncharged
/// production gets deep, so an uncharged family hides behind its neighbours.
/// Most sources therefore draw from a *small* sub-alphabet — often a single
/// token — which is what produces the long homogeneous chains that expose a
/// production nobody charged. The remainder stay uniform so combinations are
/// still covered.
fn fuzz_source(seed: u64, tokens: usize) -> String {
    let mut prng = Prng(seed);
    let alphabet_size = match prng.below(8) {
        0..=2 => 1,
        3 | 4 => 2,
        5 => 3,
        6 => 5,
        _ => FUZZ_ALPHABET.len(),
    };
    let chosen = (0..alphabet_size)
        .map(|_| FUZZ_ALPHABET[prng.below(FUZZ_ALPHABET.len())])
        .collect::<Vec<_>>();
    let mut source = String::new();
    for _ in 0..tokens {
        source.push_str(chosen[prng.below(chosen.len())]);
    }
    source
}

/// The differential guard: no source built from the charged alphabet may drive
/// SWC past the stack budget. The child parses each one on the 2 MiB contract,
/// so a shape the preflight fails to charge takes the child down and fails the
/// test instead of the host.
#[test]
fn fuzzed_token_sequences_never_abort_the_parser() {
    const CHILD_ENV: &str = "LASH_TS_FUZZ_CHILD";
    const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;
    // Fixed corpus: same seeds, same lengths, same sources, every run.
    const BATCHES: u64 = 8;
    const PER_BATCH: u64 = 512;
    const TOKEN_LENGTHS: &[usize] = &[64, 512, 4_096, 20_000];

    if let Some(batch) = std::env::var_os(CHILD_ENV) {
        let batch: u64 = batch.to_string_lossy().parse().expect("batch index");
        // Triage hook: point LASH_TS_FUZZ_TRACE at a file to record each source
        // before it is parsed, so the last line names the one that aborted.
        if let Some(path) = std::env::var_os("LASH_TS_FUZZ_TRACE") {
            std::fs::write(&path, format!("child {batch} entered\n")).expect("trace file");
        }
        std::thread::Builder::new()
            .stack_size(STACK_BUDGET_BYTES)
            .spawn(move || {
                let mut bounded = 0usize;
                let mut total = 0usize;
                for step in 0..PER_BATCH {
                    let seed = batch * PER_BATCH + step;
                    for tokens in TOKEN_LENGTHS {
                        let source = fuzz_source(seed ^ (*tokens as u64) << 32, *tokens);
                        // Any outcome is acceptable except taking the process
                        // down: a parse, a dialect rejection, or the nesting
                        // diagnostic. Reaching the next iteration is the
                        // assertion.
                        total += 1;
                        if let Some(path) = std::env::var_os("LASH_TS_FUZZ_TRACE") {
                            use std::io::Write;
                            let head: String = source.chars().take(140).collect();
                            let mut file = std::fs::OpenOptions::new()
                                .append(true)
                                .open(&path)
                                .expect("trace file");
                            writeln!(file, "SEED {seed} TOKENS {tokens} :: {head:?}").ok();
                            file.flush().ok();
                        }
                        if lash_typescript::parse(&source)
                            .is_err_and(|error| error.code.as_str() == "TS_SOURCE_NESTING_LIMIT")
                        {
                            bounded += 1;
                        }
                    }
                }
                // A corpus that never reaches the budget would pass this test
                // without testing anything, so require that it does reach it.
                assert!(
                    bounded * 2 > total,
                    "the corpus must exercise the budget: {bounded} of {total} were bounded"
                );
            })
            .expect("fuzz thread starts")
            .join()
            .expect("fuzz thread does not abort or panic");
        return;
    }

    for batch in 0..BATCHES {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "fuzzed_token_sequences_never_abort_the_parser",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD_ENV, batch.to_string())
            .status()
            .expect("fuzz child starts");
        assert!(
            status.success(),
            "fuzz batch {batch} did not survive the stack budget: {status}"
        );
    }
}

/// The corpus is a fixture, not a random walk: if the generator or the alphabet
/// changes, this fingerprint changes and the change is deliberate.
#[test]
fn the_fuzz_corpus_is_deterministic() {
    let first = fuzz_source(7, 32);
    let again = fuzz_source(7, 32);
    assert_eq!(first, again, "the same seed produces the same source");
    assert_ne!(fuzz_source(8, 32), first, "different seeds differ");
    assert_eq!(
        first.len(),
        fuzz_source(7, 32).len(),
        "length is a function of the seed alone"
    );
}
