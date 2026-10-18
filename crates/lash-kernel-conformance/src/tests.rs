use lash_kernel_doc::{Datum, parse_document, print_document};

use crate::{
    Case, Environment, Expected, ExpectedEnd, PendingRule, Shard, check_coverage, load_corpus,
};

include!("corpus_files.rs");

#[path = "contract_cases.rs"]
mod contract_cases;

mod charge;
mod protocol;
mod regions;

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

fn corpus_shards() -> Vec<Shard> {
    load_corpus(
        CORPUS_FILES
            .iter()
            .chain(LIBRARY_CORPUS_FILES)
            .chain(protocol::files())
            .copied(),
    )
    .expect("decode independently owned document shards")
}

mod rule_cases {
    include!("machine_cases.rs");
}

fn run_rule(rule: &str, text: &str) {
    let shard: Shard = serde_json::from_str(text).expect("decode rule shard");
    assert_eq!(shard.rule, rule);
    let registry = corpus_registry();
    contract_cases::verify(&shard, &registry);
    run_shard(&shard, registry);
}

pub(super) fn corpus_registry() -> lash_kernel_doc::FunctionRegistry {
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
    registry
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
fn admitted_corpus_library_identities_are_current() {
    // FIG-6558: a changed library body must not strand an admitted rule case
    // on its superseded identity before the rule's observations can run.
    let registry = corpus_registry();
    let shards = load_corpus(
        CORPUS_FILES
            .iter()
            .chain(LIBRARY_CORPUS_FILES)
            .chain(&[(
                "K-LJSON-005.json",
                include_str!("../supplements/json/K-LJSON-005.json"),
            )])
            .copied(),
    )
    .expect("decode document corpus");
    let mut missing = Vec::new();
    for shard in shards {
        for case in shard.cases {
            // Refusal cases deliberately name unavailable functions.
            if case.expected.end == ExpectedEnd::Refused {
                continue;
            }
            let document = parse_document(&case.document).expect("admitted case parses");
            for (id, name) in &document.manifest.functions {
                if registry.get(id).is_none() {
                    missing.push(format!("{} {}: {name} = @{id}", shard.rule, case.name));
                }
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
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

/// K-ID-004: the corpus's expected identities point to the actual action,
/// including a do expression, independently of the machine's executable layout.
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
                let actual = match document.node(&identity.site) {
                    Some(lash_kernel_doc::Node::Action(lash_kernel_doc::Action::Perform {
                        effect,
                        ..
                    })) => effect,
                    Some(lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Act(act))) => {
                        let lash_kernel_doc::ActKind::Perform { effect, .. } = &act.kind else {
                            panic!("{}: expected effect site is not a perform", case.name);
                        };
                        effect
                    }
                    _ => panic!("{}: expected effect site is not a perform", case.name),
                };
                assert_eq!(actual.as_str(), effect);
                if let lash_kernel_doc::kernel_internals::TaskIdentity::Spawned(spawn) =
                    &identity.task
                {
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

/// PLAN-INDEP / K-MACH-007/008: every corpus statement projects, and each
/// plan resumes the other plan's images with warm and cold prepared code.
#[test]
fn corpus_cross_resumes_after_every_statement() {
    use crate::projection::{Plan, Projector};
    let library = lash_kernel_vm::PreparedLibrary::new(std::sync::Arc::new(corpus_registry()));
    let shards = load_corpus(CORPUS_FILES.iter().chain(LIBRARY_CORPUS_FILES).copied()).unwrap();
    let mut executed = 0;
    let mut failures = Vec::new();
    for case in shards.iter().flat_map(|shard| &shard.cases) {
        let mut case = case.clone();
        case.environment.slice = 0;
        case.environment.resume = false;
        let result = (|| -> Result<(), crate::HarnessError> {
            let mut reference = Projector::default();
            let a = crate::machine::run_case_projected::<Plan<0>>(
                &library,
                &case,
                &mut |at| Ok(at.machine),
                Some(&mut reference),
            )?;
            case.expected.check(&a)?;
            reference.check_memory()?;
            for cold in [false, true] {
                let mut imported = Projector {
                    imports: Some(&reference.images),
                    cold,
                    layout: lash_kernel_vm::kernel_internals::Layout(3),
                    ..Projector::default()
                };
                let b = crate::machine::run_case_projected::<Plan<3>>(
                    &library,
                    &case,
                    &mut |at| Ok(at.machine),
                    Some(&mut imported),
                )?;
                imported.check_memory()?;
                if !crate::projection::corpus_agrees(&a, &b, &reference, &imported) {
                    return Err(crate::HarnessError(format!(
                        "cross-resume mismatch (cold={cold}): {a:?} / {b:?}"
                    )));
                }
            }
            Ok(())
        })();
        executed += 1;
        if let Err(error) = result {
            failures.push(format!("{}: {error}", case.name));
        }
    }
    println!(
        "corpus cross-resume: executed={executed}, projection after every statement, plans=0/3, warm/cold"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// FIG-6732: the same corpus retains its results, raises, effect identities,
/// order and parks when its charge meter is absent. Charge-bound refusals
/// are the charge oracle's domain and run only under Exact.
#[test]
fn charge_off_preserves_the_conformance_corpus() {
    use crate::DocumentRunner;
    use lash_kernel_vm::{Charge, KernelMachine};
    let registry = std::sync::Arc::new(corpus_registry());
    let mut exact = crate::MachineRunner::<KernelMachine>::new(registry.clone());
    let mut off = crate::MachineRunner::<KernelMachine>::new(registry).with_charge(Charge::Off);
    let mut executed = 0;
    let mut failures = Vec::new();
    for shard in corpus_shards() {
        for case in shard.cases {
            if matches!(&case.expected.end, ExpectedEnd::Bound { name, .. } if name == "charge") {
                continue;
            }
            let result = (|| -> Result<(), crate::HarnessError> {
                let mut expected = exact.observe(&case)?;
                let actual = off.observe(&case)?;
                if actual.charged.is_some() {
                    return Err(crate::HarnessError("Off returned a charge meter".into()));
                }
                expected.charged = None;
                if actual != expected {
                    return Err(crate::HarnessError(format!("{expected:?} != {actual:?}")));
                }
                Ok(())
            })();
            executed += 1;
            if let Err(error) = result {
                failures.push(format!("{} {}: {error}", shard.rule, case.name));
            }
        }
    }
    println!("charge-off corpus: executed={executed}");
    assert!(executed > 0);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// FIG-6731: Off keeps the existing corpus's semantic results, errors,
/// effects, charge and parks while doing no census accounting. Memory-bound
/// cases name the rule intentionally disabled and remain on the metered gate.
#[test]
fn memory_off_preserves_the_corpus_without_census_work() {
    use crate::DocumentRunner;
    let mut runner = crate::MachineRunner::<lash_kernel_vm::KernelMachine>::new(
        std::sync::Arc::new(corpus_registry()),
    );
    let mut checked = 0;
    let mut memory_bound_cases = 0;
    for shard in corpus_shards() {
        for mut case in shard.cases {
            if matches!(&case.expected.end, ExpectedEnd::Bound { name, .. } if name == "memory") {
                memory_bound_cases += 1;
                continue;
            }
            let metered = runner
                .observe(&case)
                .unwrap_or_else(|error| panic!("{} {}: {error}", shard.rule, case.name));
            case.environment.bounds.memory_meter = lash_kernel_vm::MemoryMeter::Off;
            lash_kernel_vm::perf_witness::enable_work();
            let off = runner.observe(&case);
            let work = lash_kernel_vm::perf_witness::take_work().unwrap();
            lash_kernel_vm::perf_witness::disable_work();
            assert_eq!(
                off.unwrap_or_else(|error| panic!("{} {}: {error}", shard.rule, case.name)),
                metered,
                "{} {}",
                shard.rule,
                case.name
            );
            assert_eq!(
                (
                    work.heap.accounting_additions,
                    work.heap.refinement_attempts,
                ),
                (0, 0),
                "{} {}",
                shard.rule,
                case.name
            );
            checked += 1;
        }
    }
    assert!(checked > 0);
    println!("memory-off corpus: compared={checked} memory-bound-cases={memory_bound_cases}");
}
