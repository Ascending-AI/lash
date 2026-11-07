//! ISA-PREFIX-EXACT over admitted programs, using the production machine and
//! the testing-only statement executor. No fixture computes expected values.
use super::*;
use crate::laws::embedder::{self, Embedder, ROOMY, Source};

fn pages() -> Box<dyn PageSource> {
    Box::new(Source::default())
}

fn case(text: &str) -> QualificationCase {
    let embedder = Embedder::new(text);
    let (program, _) = embedder.program(true);
    qualification_case(
        program,
        embedder::Setup::default().start,
        ROOMY,
        pages,
        Box::new([]),
    )
    .with_answers(embedder::answer)
}

fn report(name: &str, case: &QualificationCase) -> usize {
    let mut failures = 0;
    let source = match source_oracle(case) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("ORACLE_ADAPTER {name} source {error}");
            failures += 1;
            error.reached().to_vec()
        }
    };
    for layout in [IsaLayout::Generic, IsaLayout::Specialized] {
        let isa = match isa_prefixes(case, layout) {
            Ok(isa) => isa,
            Err(error) => {
                eprintln!("ORACLE_ADAPTER {name} {layout:?} {error}");
                failures += 1;
                error.reached().to_vec()
            }
        };
        for (prefix, field, expected, actual) in comparison::disagreements(&source, &isa) {
            eprintln!(
                "ORACLE_DIFF {}",
                serde_json::json!({"case":name,"layout":format!("{layout:?}"),"prefix":prefix,"field":field,"source":expected,"region":actual})
            );
            failures += 1;
        }
        // All returned-prefix differences above still fail the law. Also report
        // full observations at matching host parks so slice granularity cannot
        // obscure an independently reached charge/graph/continuation defect.
        let host_prefixes = |trace: &[PrefixSnapshot]| {
            trace
                .iter()
                .filter(|snapshot| {
                    let stop = &snapshot.canonical().unwrap().stop;
                    stop == "initial" || stop.starts_with("Parked") || stop.starts_with("Ended")
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let source_host = host_prefixes(&source);
        let isa_host = host_prefixes(&isa);
        for (prefix, field, expected, actual) in comparison::disagreements(&source_host, &isa_host)
        {
            eprintln!(
                "ORACLE_HOST_DIFF {}",
                serde_json::json!({"case":name,"layout":format!("{layout:?}"),"prefix":prefix,"field":field,"source":expected,"region":actual})
            );
        }
        eprintln!(
            "ORACLE_TRACE {name} {layout:?} source={} region={}",
            source.len(),
            isa.len()
        );
        for (side, trace) in [("source", &source), ("region", &isa)] {
            for (prefix, snapshot) in trace.iter().enumerate() {
                let state = snapshot.canonical().unwrap();
                if state.stop.starts_with("Parked") || state.stop.starts_with("Ended") {
                    eprintln!(
                        "ORACLE_STOP {}",
                        serde_json::json!({"case":name,"layout":format!("{layout:?}"),"side":side,"prefix":prefix,"charge":state.meters.charged,"memory":state.meters.memory,"stop":state.stop})
                    );
                }
            }
        }
    }
    failures
}

/// FIG-7634: materialization must match source values, continuation, graph,
/// ordered charge events and itemized holdings at every returned prefix.
#[test]
fn admitted_qualification_corpus_matches_source_at_every_prefix() {
    let mut failures = 0;
    for loops in 0..=3 {
        for input in [-2, 0, 7, i64::MAX - 2, i64::MAX] {
            for alias in [false, true] {
                for cycle in [false, true] {
                    let text = format!(
                        "main {{ let a = [1] let b = {} {} for i in [0, 1, 2, 3] {{ if num.lt(i, {loops}) {{ set b[0] = num.add(b[0], {input}) }} }} let wait = perform 0(7) as Any return (a, b) }}",
                        if alias { "a" } else { "[2]" },
                        if cycle { "set a[1] = a" } else { "" }
                    );
                    let name = format!("loops={loops},input={input},alias={alias},cycle={cycle}");
                    failures += report(&name, &case(&text));
                }
            }
        }
    }
    assert_eq!(
        failures, 0,
        "independent prefix disagreements are printed above"
    );
}

/// FIG-7669's existing cold-list program is a required red-side witness:
/// the comparison must expose the first-park charge and memory discrepancy.
#[test]
fn cold_list_prefixes_match_source() {
    let case = case(
        "main { let xs = work.fill(20000) let a = perform 0(1) as Any set xs[10003] = 7 let b = perform 0(2) as Any return xs[10003] }",
    ).with_park_reopen();
    assert_eq!(
        report("cold-list-20000", &case),
        0,
        "cold-list independent prefix disagreements"
    );
}
