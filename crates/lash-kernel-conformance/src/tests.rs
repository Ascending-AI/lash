use lash_kernel_doc::{Datum, parse_document, print_document};

use crate::{
    Case, Environment, Expected, ExpectedEnd, PendingRule, Shard, check_coverage, load_corpus,
};

include!("corpus_files.rs");

// These three owning-library shards fill rules this corpus does not own.
const LIBRARY_CORPUS_FILES: &[(&str, &str)] = &[
    (
        "K-KEY-002.json",
        include_str!("../../lash-kernel-lib/corpus/numbers/K-KEY-002.json"),
    ),
    (
        "K-KEY-004.json",
        include_str!("../../lash-kernel-lib/corpus/numbers/K-KEY-004.json"),
    ),
    (
        "K-VAL-026.json",
        include_str!("../../lash-kernel-lib/corpus/numbers/K-VAL-026.json"),
    ),
];

// Additional cases for already-owned rules run without claiming a second owner.
const LIBRARY_SUPPLEMENT_FILES: &[(&str, &str)] = &[
    (
        "K-KEY-003.json",
        include_str!("../../lash-kernel-lib/corpus/numbers/K-KEY-003.json"),
    ),
    (
        "K-KEY-005.json",
        include_str!("../../lash-kernel-lib/corpus/numbers/K-KEY-005.json"),
    ),
    (
        "K-VAL-001.json",
        include_str!("../../lash-kernel-lib/corpus/numbers/K-VAL-001.json"),
    ),
];

fn corpus_shards() -> Vec<Shard> {
    load_corpus(CORPUS_FILES.iter().chain(LIBRARY_CORPUS_FILES).copied())
        .expect("decode independently owned document shards")
}

mod rule_cases {
    include!("machine_cases.rs");
}

fn run_rule(rule: &str, text: &str) {
    let shard: Shard = serde_json::from_str(text).expect("decode rule shard");
    assert_eq!(shard.rule, rule);
    let mut registry = lash_kernel_doc::FunctionRegistry::new();
    lash_kernel_vm::register_machine_functions(&mut registry).expect("register kernel functions");
    lash_kernel_lib::register_numbers(&mut registry).expect("register numeric natives");
    lash_kernel_lib::register_text_json(&mut registry).expect("register text and JSON natives");
    lash_kernel_lib::register_collections(&mut registry).expect("register collection functions");
    lash_ext_regex_ecma::register(
        &mut registry,
        &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(4)),
    )
    .expect("register regex natives");
    run_shard(&shard, registry);
}

fn run_shard(shard: &Shard, registry: lash_kernel_doc::FunctionRegistry) {
    let mut runner =
        crate::MachineRunner::<lash_kernel_vm::KernelMachine>::new(std::sync::Arc::new(registry));
    let mut failures = Vec::new();
    for case in &shard.cases {
        if let Err(error) = crate::check_case(&mut runner, case) {
            failures.push(format!("{}: {error}", case.name));
        }
    }
    assert!(
        failures.is_empty(),
        "{}:\n{}",
        shard.rule,
        failures.join("\n")
    );
}

#[test]
fn numeric_library_documents_obey_their_kernel_rules() {
    let shards = load_corpus(
        LIBRARY_CORPUS_FILES
            .iter()
            .chain(LIBRARY_SUPPLEMENT_FILES)
            .copied(),
    )
    .expect("decode numeric library documents");
    for shard in &shards {
        let mut registry = lash_kernel_doc::FunctionRegistry::new();
        lash_kernel_vm::register_machine_functions(&mut registry)
            .expect("register kernel functions");
        lash_kernel_lib::register_numbers(&mut registry).expect("register numeric natives");
        run_shard(shard, registry);
    }
}

/// K-LJSON-005: the root need not be a heap value for a cycle to be refused.
#[test]
fn json_stringify_refuses_cycle_from_immutable_tuple() {
    let shard: Shard = serde_json::from_str(include_str!("../supplements/json/K-LJSON-005.json"))
        .expect("decode immutable-root stringify case");
    assert!(
        crate::rule_ids(include_str!("../../../docs/kernel/library-text-json.md"))
            .contains(&shard.rule)
    );
    let mut registry = lash_kernel_doc::FunctionRegistry::new();
    lash_kernel_lib::register_text_json(&mut registry).expect("register JSON native");
    run_shard(&shard, registry);
}

#[test]
fn embedder_fan_out_parks_and_resumes() {
    let answers = [1, 2]
        .into_iter()
        .map(|value| kernel_embedder::Answer {
            effect: "echo".into(),
            args: vec![Datum::Int(value.into())],
            outcome: lash_kernel_vm::Outcome::Completed(Datum::Int(value.into())),
        })
        .collect();
    let report = kernel_embedder::embed::<lash_kernel_vm::KernelMachine>(
        kernel_embedder::FAN_OUT,
        std::sync::Arc::new(lash_kernel_doc::FunctionRegistry::new()),
        answers,
    )
    .expect("real kernel machine parks, exports, rebuilds, and resumes");
    assert_eq!(
        report.prints,
        vec![Datum::Int(2.into()), Datum::Int(1.into())]
    );
    let lash_kernel_vm::End::Finished(end) = report.end else {
        panic!("fan-out ended unexpectedly: {:?}", report.end);
    };
    assert_eq!(
        end.result,
        Datum::List(vec![Datum::Int(1.into()), Datum::Int(2.into())])
    );
    assert_eq!(report.requests.len(), 2);
    assert_eq!(report.parks, 1);
    assert_eq!(report.resumes, 1);
}

fn case(name: &str) -> Case {
    Case {
        name: name.into(),
        document: "kernel 1\nnumbers by_spelling\nmain { finish null }".into(),
        environment: Environment::default(),
        expected: Expected {
            prints: Vec::new(),
            end: ExpectedEnd::Finished(Datum::Null),
            trace: Vec::new(),
            charged: None,
            parks: None,
        },
    }
}

#[test]
fn coverage_every_defined_rule_has_a_case() {
    let rules = "- **K-FORM-001.** A form.\n- **K-TASK-001.** A task.\n";
    let mut shards = vec![Shard {
        rule: "K-FORM-001".into(),
        cases: vec![case("forms are closed")],
    }];
    let missing = check_coverage(rules, &shards, &[]).expect_err("a rule without a case is red");
    assert_eq!(missing.missing, ["K-TASK-001".into()].into_iter().collect());
    shards.push(Shard {
        rule: "K-TASK-001".into(),
        cases: vec![case("tasks share a run")],
    });
    assert_eq!(check_coverage(rules, &shards, &[]), Ok(()));
}

#[test]
fn coverage_refuses_unknown_empty_and_duplicate_shards() {
    let rules = "- **K-FORM-001.** A form (K-TASK-001 is a reference).\n";
    let shards = vec![
        Shard {
            rule: "K-FORM-001".into(),
            cases: vec![case("closed forms")],
        },
        Shard {
            rule: "K-FORM-001".into(),
            cases: Vec::new(),
        },
        Shard {
            rule: "K-TASK-001".into(),
            cases: vec![case("unknown task rule")],
        },
    ];
    let error =
        check_coverage(rules, &shards, &[]).expect_err("unknown and empty rules are refused");
    assert!(error.unknown.contains("K-TASK-001"));
    assert!(error.malformed.contains("K-FORM-001"));
}

/// The coverage ratchet ruling: every rule has a case or a named owner.
#[test]
fn coverage_pending_owners_partition_the_rules() {
    let rules = "- **K-FORM-001.** A form.\n- **K-TASK-001.** A task.\n";
    let shards = vec![Shard {
        rule: "K-FORM-001".into(),
        cases: vec![case("forms are closed")],
    }];
    let pending = vec![PendingRule {
        rule: "K-TASK-001".into(),
        owner: "KPARK (FIG-5696)".into(),
    }];
    assert_eq!(check_coverage(rules, &shards, &pending), Ok(()));
    let absent = check_coverage(rules, &shards, &[]).expect_err("neither case nor owner");
    assert!(absent.missing.contains("K-TASK-001"));
    let mut complete = shards.clone();
    complete.push(Shard {
        rule: "K-TASK-001".into(),
        cases: vec![case("tasks share a run")],
    });
    let both = check_coverage(rules, &complete, &pending).expect_err("both case and owner");
    assert!(both.overlap.contains("K-TASK-001"));
    assert_eq!(check_coverage(rules, &complete, &[]), Ok(()));
    for rows in [
        vec![PendingRule {
            rule: "K-TASK-001".into(),
            owner: " ".into(),
        }],
        vec![pending[0].clone(), pending[0].clone()],
    ] {
        assert!(
            !check_coverage(rules, &shards, &rows)
                .expect_err("owner must be named exactly once")
                .malformed
                .is_empty()
        );
    }
    let unknown = [PendingRule {
        rule: "K-FUTURE-001".into(),
        owner: "KCONF (FIG-5702)".into(),
    }];
    assert!(
        check_coverage(rules, &complete, &unknown)
            .expect_err("pending rules must exist")
            .unknown
            .contains("K-FUTURE-001")
    );
}

#[test]
fn corpus_kernel_text_round_trips() {
    let shards = corpus_shards();
    assert!(!shards.is_empty(), "the corpus must contain cases");
    let mut failures = Vec::new();
    for shard in shards {
        for case in shard.cases {
            if case.expected.end == ExpectedEnd::Refused {
                continue;
            }
            let document = match parse_document(&case.document) {
                Ok(document) => document,
                Err(error) => {
                    failures.push(format!("{} {}: {error}", shard.rule, case.name));
                    continue;
                }
            };
            let printed = print_document(&document);
            assert_eq!(
                parse_document(&printed).expect("parse canonical kernel text"),
                document,
                "{} {}",
                shard.rule,
                case.name
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn corpus_every_semantic_rule_has_a_case_or_owner() {
    let rules = include_str!("../../../docs/kernel/semantics.md");
    let shards = corpus_shards();
    let pending: Vec<PendingRule> = serde_json::from_str(include_str!("../pending.json"))
        .expect("decode committed pending owners");
    assert_eq!(check_coverage(rules, &shards, &pending), Ok(()));
}

/// K-DOC-006: malformed input is bounded before execution. No legacy wire
/// width or instruction-pointer format is involved.
#[test]
fn bounded_kernel_text_and_json_refuse_overdeep_forms() {
    let shards = load_corpus(CORPUS_FILES.iter().copied()).expect("decode corpus");
    let shard = shards
        .iter()
        .find(|shard| shard.rule == "K-DOC-006")
        .expect("bounded encoding shard");
    for case in &shard.cases {
        let error = parse_document(&case.document).expect_err("overdeep kernel text is refused");
        assert!(matches!(
            error.reason,
            lash_kernel_doc::ParseErrorReason::TooDeep { .. }
        ));
    }
    let mut block = String::from("[]");
    for _ in 0..200 {
        block = format!(
            "[{{\"if\":{{\"condition\":{{\"literal\":{{\"bool\":true}}}},\"then_block\":{block}}}}}]"
        );
    }
    let json =
        format!("{{\"manifest\":{{\"kernel\":1,\"numbers\":\"by_spelling\"}},\"main\":{block}}}");
    assert!(matches!(
        lash_kernel_doc::Document::from_json(&json),
        Err(lash_kernel_doc::DecodeError::TooDeep { .. })
    ));
}

/// K-DOC-004: every nested object and enum is closed. There is no legacy
/// FutureShape/Codec distinction to preserve.
#[test]
fn kernel_json_refuses_unknown_nested_forms() {
    let document = parse_document("kernel 1\nnumbers by_spelling\nmain { finish null }")
        .expect("parse document");
    let mut json: serde_json::Value =
        serde_json::from_str(&document.to_json().expect("encode document")).expect("parse JSON");
    json["main"][0]["finish"]["value"] = serde_json::json!({"future_expression": {}});
    assert!(matches!(
        lash_kernel_doc::Document::from_json(&json.to_string()),
        Err(lash_kernel_doc::DecodeError::Invalid { .. })
    ));
}

/// K-ID-004: the corpus's expected identities point to the actual action in
/// the typed document, independently of the machine's executable layout.
#[test]
fn expected_effect_sites_address_document_actions() {
    let shards = load_corpus(CORPUS_FILES.iter().copied()).expect("decode corpus");
    let mut identities = 0;
    for case in shards.iter().flat_map(|shard| &shard.cases) {
        let document = parse_document(&case.document);
        for trace in &case.expected.trace {
            if let crate::Trace::Effect {
                identity: Some(identity),
                effect,
                ..
            } = trace
            {
                let document = document.as_ref().expect("identity case parses");
                let Some(lash_kernel_doc::Node::Action(lash_kernel_doc::Action::Perform {
                    effect: actual,
                    ..
                })) = document.node(&identity.site)
                else {
                    panic!("{}: expected effect site is not a perform", case.name);
                };
                assert_eq!(actual.as_str(), effect);
                if let lash_kernel_doc::TaskIdentity::Spawned(spawn) = &identity.task {
                    assert!(matches!(
                        document.node(&spawn.site),
                        Some(lash_kernel_doc::Node::Action(
                            lash_kernel_doc::Action::Spawn { .. }
                        ))
                    ));
                }
                identities += 1;
            }
        }
    }
    assert!(
        identities >= 3,
        "main, spawn, and resume identities are pinned"
    );
}
