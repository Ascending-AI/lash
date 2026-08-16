//! The load-bearing proof that parsing a TypeScript cell cannot take the host
//! process down.
//!
//! Five rounds of review closed this property one shape at a time: each round's
//! guards were correct about the axis they modelled, and each following round
//! found an abort one definition to the side of it — a grammar family nobody
//! had enumerated, then an identifier the scanner tokenised differently from
//! SWC, then a contextual keyword classified by the wrong predicate. The
//! pattern is structural: the source-nesting preflight is a second
//! implementation of SWC's lexer, and any disagreement between the two silently
//! disarms a charge.
//!
//! So the **stack** no longer rests on that agreement. The source is bounded,
//! and the parse runs on a thread whose stack is reserved in proportion to the
//! source, with margin over the worst frame cost ever measured. The preflight
//! remains — a source-level `TS_SOURCE_NESTING_LIMIT` is a far better diagnostic
//! than a parser-depth error, and rejecting before the parse keeps a
//! pathological cell cheap — but it is no longer what stands between an
//! LLM-authored cell and a stack overflow.
//!
//! **Memory is a different story, and the preflight still carries it.** These
//! tests assert that nothing aborts, and nothing does; they do not bound
//! allocation, and with the preflight disabled a 64 KiB cell can commit tens of
//! gigabytes. SWC's duplicate-label check is quadratic in memory, so a cell of
//! one repeated label — `a:` to the bound — peaks at about 37 GB, which is why
//! the duplicate-label shapes below are filled to 8 KiB instead: that is a
//! workaround in the corpus, not a bound in the product. On the shipping path
//! there is no reachable vector, because the preflight rejects every such shape
//! at 28 units before the parse begins and the worst peak across 164
//! adversarial shapes is 17 MB. But the honest statement of the guarantee is:
//! **stack is bounded arithmetically, memory is bounded by the preflight being
//! right.** Bounding allocation — the natural closure is parsing in a
//! subprocess, where both axes come under one limit — is the work that would
//! retire the second half.
//!
//! These tests demonstrate exactly that by **disabling the preflight** and
//! running every shape that aborted in any round, plus the fuzzer's corpus,
//! through the remaining machinery. Every one must come back as a diagnostic or
//! a successful parse.

/// Every shape that aborted the process in rounds 1 through 6, at the depth
/// that a 64 KiB source allows — far past the depth that originally aborted.
fn abort_corpus() -> Vec<(String, String)> {
    let cap = lash_typescript::MAX_SOURCE_BYTES;
    // Repeat each unit until the source nearly fills the cap, so every shape is
    // driven as deep as an accepted cell can drive it.
    let fill_to = |limit: usize, prefix: &str, unit: &str, suffix: &str, closer: &str| {
        let per = unit.len() + closer.len();
        let repeats = (limit - prefix.len() - suffix.len() - 16) / per.max(1);
        format!(
            "{prefix}{}{suffix}{}",
            unit.repeat(repeats),
            closer.repeat(repeats)
        )
    };
    let fill = |prefix: &str, unit: &str, suffix: &str, closer: &str| {
        fill_to(cap, prefix, unit, suffix, closer)
    };
    // Repeating one label name is quadratic in SWC's duplicate-label check, not
    // in anything this guard is about, so those shapes fill a smaller source —
    // still thousands of levels, and orders of magnitude past the 200 that
    // originally aborted. The distinct-label shape below carries the full bound.
    let duplicate_label_limit = 8 * 1024;
    // Grouped by the review round that found each shape.
    let mut corpus: Vec<(String, String)> = Vec::with_capacity(40);
    // Round 1: prefix operators, ternary and binary chains, delimiters.
    corpus.push(("round1-not".into(), fill("finish(", "!", "1);", "")));
    corpus.push((
        "round1-typeof".into(),
        fill("finish(", "typeof ", "1);", ""),
    ));
    corpus.push(("round1-minus".into(), fill("finish(", "- ", "1);", "")));
    corpus.push(("round1-ternary".into(), fill("finish(", "1?1:", "1);", "")));
    corpus.push(("round1-binary".into(), fill("finish(1", "+1", ");", "")));
    corpus.push(("round1-paren".into(), fill("finish(", "(", "1", ")")));
    corpus.push(("round1-bracket".into(), fill("const x = ", "[", "1", "]")));
    corpus.push(("round1-brace".into(), fill("const x = ", "{a:", "1", "}")));
    corpus.push((
        "wpb-nested-array-callback".into(),
        fill("finish([1].map(", "x => [x].filter(", "x => true", ")"),
    ));
    // Round 3: prefix keywords across lines, postfix chains.
    corpus.push((
        "round3-typeof-newline".into(),
        fill("const x = ", "typeof\n", "1;", ""),
    ));
    corpus.push((
        "round3-void-newline".into(),
        fill("const x = ", "void\n", "1;", ""),
    ));
    corpus.push((
        "round3-new-newline".into(),
        fill("const x = ", "new\n", "1;", ""),
    ));
    corpus.push((
        "round3-delete-newline".into(),
        fill("const x = ", "delete\n", "1;", ""),
    ));
    corpus.push((
        "round3-call-chain".into(),
        fill("const x = a", "(1)", ";", ""),
    ));
    corpus.push((
        "round3-subscript-chain".into(),
        fill("const x = a", "[0]", ";", ""),
    ));
    corpus.push((
        "round3-tagged-template".into(),
        fill("const x = a", "`x`", ";", ""),
    ));
    corpus.push((
        "round3-tagged-template-newline".into(),
        fill("const x = a\n", "`x`\n", ";", ""),
    ));
    // Round 4: labels, casts, type operators, for-header separators.
    let distinct_labels = (0..(cap - 32) / 8)
        .map(|index| format!("l{index:06}:"))
        .collect::<String>();
    corpus.push(("round4-label".into(), format!("{distinct_labels}1;")));
    corpus.push((
        "round4-label-duplicate".into(),
        fill_to(duplicate_label_limit, "", "a:", "1;", ""),
    ));
    corpus.push((
        "round4-as".into(),
        fill("const x = 1", " as number", ";", ""),
    ));
    corpus.push((
        "round4-satisfies".into(),
        fill("const x = 1", " satisfies number", ";", ""),
    ));
    corpus.push((
        "round4-keyof".into(),
        fill("const x: ", "keyof ", "number = 1;", ""),
    ));
    corpus.push((
        "round4-for-comment".into(),
        fill("", "for (;;) // c\n", "1;", ""),
    ));
    // Round 5: non-ASCII identifiers in label position.
    corpus.push((
        "round5-unicode-label".into(),
        fill_to(duplicate_label_limit, "", "a\u{e9}:", "1;", ""),
    ));
    corpus.push((
        "round5-cjk-label".into(),
        fill_to(duplicate_label_limit, "", "a\u{4e2d}:", "1;", ""),
    ));
    corpus.push((
        "round5-escape-label".into(),
        fill_to(duplicate_label_limit, "", "a\\u00e9:", "1;", ""),
    ));
    // The true worst case for the reservation, and the shape the round-7
    // verification used to falsify the original derivation: an *unclosed*
    // delimiter recurses one level per source byte, and is simultaneously the
    // most expensive shape per level. These fill the bound exactly, so the
    // reservation's real margin is measured here rather than asserted in a
    // comment.
    corpus.push(("worst-open-paren".into(), "(".repeat(cap - 1)));
    corpus.push(("worst-open-bracket".into(), "[".repeat(cap - 1)));
    corpus.push(("worst-open-brace".into(), "{".repeat(cap - 1)));
    corpus.push((
        "worst-open-generic".into(),
        format!("const x: {}", "A<".repeat(cap / 2 - 8)),
    ));
    corpus.push((
        "worst-open-template".into(),
        format!("`{}", "${`".repeat(cap / 3 - 4)),
    ));
    corpus.push((
        "worst-open-paren-type".into(),
        format!("const x: {}", "(".repeat(cap - 12)),
    ));
    corpus.push((
        "worst-open-paren-unicode-tail".into(),
        format!("{}\u{e9}", "(".repeat(cap - 4)),
    ));
    // Round 6: contextual keywords in label position.
    corpus.push((
        "round6-type-label".into(),
        fill_to(duplicate_label_limit, "", "type:", "1;", ""),
    ));
    corpus.push((
        "round6-of-label".into(),
        fill_to(duplicate_label_limit, "", "of:", "1;", ""),
    ));
    corpus.push((
        "round6-let-label".into(),
        fill_to(duplicate_label_limit, "", "let:", "1;", ""),
    ));
    corpus.push((
        "round6-alternating-label".into(),
        fill_to(duplicate_label_limit, "", "type:of:", "1;", ""),
    ));
    corpus.push((
        "round6-keyof-label".into(),
        fill_to(duplicate_label_limit, "", "keyof:", "1;", ""),
    ));
    corpus
}

/// With the preflight gone, every shape that ever aborted must still come back
/// as a value — a parse, or any named diagnostic. Nothing may reach the end of
/// the stack.
#[test]
fn the_abort_corpus_survives_without_the_preflight() {
    const CHILD_ENV: &str = "LASH_TS_NO_PREFLIGHT_CHILD";
    if let Some(wanted) = std::env::var_os(CHILD_ENV) {
        let wanted = wanted.to_string_lossy().to_string();
        let (name, source) = abort_corpus()
            .into_iter()
            .find(|(name, _)| *name == wanted)
            .expect("known corpus shape");
        assert!(
            source.len() <= lash_typescript::MAX_SOURCE_BYTES,
            "{name} must fit the accepted source size"
        );
        // The assertion is that this returns at all.
        match lash_typescript::parse_without_nesting_preflight(&source) {
            Ok(_) => {}
            Err(error) => assert!(
                !error.code.as_str().is_empty(),
                "{name} must carry a named diagnostic"
            ),
        }
        return;
    }

    // One child per shape, all in flight at once: each one parses a source that
    // fills the accepted bound, which is slow enough to be worth the
    // parallelism. The parent watches for a signal, the only way this can fail.
    let children = abort_corpus()
        .into_iter()
        .map(|(name, _)| {
            let child =
                std::process::Command::new(std::env::current_exe().expect("test executable"))
                    .args([
                        "the_abort_corpus_survives_without_the_preflight",
                        "--exact",
                        "--nocapture",
                    ])
                    .env(CHILD_ENV, &name)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .expect("corpus child starts");
            (name, child)
        })
        .collect::<Vec<_>>();
    for (name, mut child) in children {
        let status = child.wait().expect("corpus child finishes");
        assert!(
            status.success(),
            "{name} did not survive without the preflight: {status}"
        );
    }
}

/// The same for the fuzzer's generated sources: with the preflight disabled,
/// none of them may reach the end of the stack either.
#[test]
fn fuzzed_sources_survive_without_the_preflight() {
    const CHILD_ENV: &str = "LASH_TS_NO_PREFLIGHT_FUZZ_CHILD";
    const SOURCES: u64 = 24;
    if let Some(batch) = std::env::var_os(CHILD_ENV) {
        let batch: u64 = batch.to_string_lossy().parse().expect("batch");
        for step in 0..SOURCES {
            let source = fuzz_source(batch * SOURCES + step, 6_000);
            if source.len() > lash_typescript::MAX_SOURCE_BYTES {
                continue;
            }
            let _ = lash_typescript::parse_without_nesting_preflight(&source);
        }
        return;
    }

    let children = (0..8)
        .map(|batch| {
            let child =
                std::process::Command::new(std::env::current_exe().expect("test executable"))
                    .args([
                        "fuzzed_sources_survive_without_the_preflight",
                        "--exact",
                        "--nocapture",
                    ])
                    .env(CHILD_ENV, batch.to_string())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .expect("fuzz child starts");
            (batch, child)
        })
        .collect::<Vec<_>>();
    for (batch, mut child) in children {
        let status = child.wait().expect("fuzz child finishes");
        assert!(
            status.success(),
            "fuzz batch {batch} did not survive without the preflight: {status}"
        );
    }
}

/// A source over the registered bound rejects by name, before any parsing.
#[test]
fn oversized_sources_reject_by_name() {
    let source = format!(
        "const x = '{}';",
        "a".repeat(lash_typescript::MAX_SOURCE_BYTES)
    );
    let error = lash_typescript::compile(&source).expect_err("an oversized source must reject");
    assert_eq!(error.code.as_str(), "TS_SOURCE_TOO_LARGE");
    // One byte under the bound is still an ordinary program.
    let filler = lash_typescript::MAX_SOURCE_BYTES - "const x = '';finish(x);".len();
    lash_typescript::compile(&format!("const x = '{}';finish(x);", "a".repeat(filler)))
        .expect("a source at the bound compiles");
}

#[test]
fn newly_accepted_constructs_survive_without_the_preflight() {
    let depth = 500;
    let sources = [
        format!("const x = a{};", "?.b".repeat(depth)),
        format!(
            "const {}x{} = {};",
            "[".repeat(depth),
            "]".repeat(depth),
            "[".repeat(depth) + "1" + &"]".repeat(depth)
        ),
        format!(
            "const x = {}[1]{};",
            "[...".repeat(depth),
            "]".repeat(depth)
        ),
        format!(
            "let x=0; finish({}x{});",
            "(~".repeat(depth),
            ")".repeat(depth)
        ),
        format!(
            "{}finish(1);{}",
            "do { switch(1) { case 1: ".repeat(depth),
            "break; } } while(false);".repeat(depth)
        ),
        format!(
            "finish({}0{});",
            "new Date(".repeat(depth),
            ")".repeat(depth)
        ),
        format!(
            "enum E {{ A = {}1{} }} finish(E.A);",
            "(".repeat(depth),
            ")".repeat(depth)
        ),
    ];
    for source in sources {
        assert!(source.len() <= lash_typescript::MAX_SOURCE_BYTES);
        match lash_typescript::parse_without_nesting_preflight(&source) {
            Ok(_) => {}
            Err(error) => assert!(!error.code.as_str().is_empty()),
        }
    }
}

// The fuzz generator, kept in step with `grammar_coverage.rs` by construction:
// both draw from the same deterministic stream.
struct Prng(u64);

impl Prng {
    fn next(&mut self) -> u64 {
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

const ATOMS: &[&str] = &[
    "a",
    "a1",
    "a\u{e9}",
    "a\u{4e2d}",
    "a\\u00e9",
    "type",
    "of",
    "let",
    "keyof",
    "readonly",
    "as",
    "1_0",
    "\u{e9}",
];

const COMBINERS: &[&str] = &[
    ":",
    "(1)",
    "[0]",
    ".a",
    " as number",
    "`x`",
    "!",
    "\u{2028}",
    "\n",
    ";",
    " ",
    "?",
    "+",
    "(",
    ")",
    "{",
    "}",
    "if (1) ",
    "typeof ",
];

fn fuzz_source(seed: u64, tokens: usize) -> String {
    let mut prng = Prng(seed);
    let atom = ATOMS[prng.below(ATOMS.len())];
    let combiner = COMBINERS[prng.below(COMBINERS.len())];
    let chosen: Vec<String> = if prng.below(4) == 0 {
        vec![atom.to_string(), combiner.to_string()]
    } else {
        vec![format!("{atom}{combiner}")]
    };
    let mut source = String::new();
    for _ in 0..tokens {
        source.push_str(&chosen[prng.below(chosen.len())]);
    }
    source
}

/// When the host cannot give the parser its reservation, that is a resource
/// failure and must be reported as one.
///
/// The reservation for a cap-sized cell exceeds 2 GiB of address space, so a
/// host with a tighter `RLIMIT_AS` cannot parse a large cell at all. Failing
/// closed is right; failing closed as though the *program* were malformed is
/// not, because it sends whoever reads the diagnostic to debug the cell instead
/// of the limit.
#[test]
fn an_unavailable_reservation_is_a_resource_diagnostic() {
    const CHILD_ENV: &str = "LASH_TS_RLIMIT_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        // A source small enough to need only the base reservation still parses
        // under the limit; only the large one cannot be given its stack.
        lash_typescript::validate("finish(1);").expect("a small cell parses under the limit");
        let large = format!(
            "const x = '{}';finish(x);",
            "a".repeat(lash_typescript::MAX_SOURCE_BYTES - 32)
        );
        let error = lash_typescript::validate(&large)
            .expect_err("the reservation cannot be met under this limit");
        assert_eq!(
            error.code.as_str(),
            "TS_PARSE_RESOURCES_UNAVAILABLE",
            "a host resource failure must not be reported as a program defect: {error}"
        );
        return;
    }

    let executable = std::env::current_exe().expect("test executable");
    let command = format!(
        "ulimit -v 2097152 && exec {} an_unavailable_reservation_is_a_resource_diagnostic --exact --nocapture",
        executable.display()
    );
    let output = std::process::Command::new("bash")
        .args(["-c", &command])
        .env(CHILD_ENV, "1")
        .output()
        .expect("the limited child starts");
    assert!(
        output.status.success(),
        "the address-space-limited child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// RegExp parsing has its own bounded preflight and must fail closed before a
/// hostile literal can drive the backing parser or executor into an abort.
#[test]
fn regexp_size_depth_and_giant_quantifiers_fail_without_aborting() {
    let too_long = format!("/{}/;", "a".repeat(4_097));
    let error = lash_typescript::validate(&too_long).expect_err("long RegExp must reject");
    assert_eq!(error.code.as_str(), "TS_REGEX_PATTERN_TOO_LONG");

    let too_deep = format!("/{}a{}/;", "(".repeat(33), ")".repeat(33));
    let error = lash_typescript::validate(&too_deep).expect_err("deep RegExp must reject");
    assert_eq!(error.code.as_str(), "TS_REGEX_PATTERN_NESTING_LIMIT");

    for pattern in [
        "/a{1,999999999999999999999999999999}/;",
        "/(?:a{999999999999999999999999999999})+b/;",
        "/(?=(a+)+$)a{999999999999999999999999999999}/;",
    ] {
        match lash_typescript::validate(pattern) {
            Ok(()) => {}
            Err(error) => assert!(
                error.code.as_str().starts_with("TS_REGEX_"),
                "hostile RegExp has a named diagnostic: {error}"
            ),
        }
    }
}
