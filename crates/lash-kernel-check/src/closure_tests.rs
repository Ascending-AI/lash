//! LINK-EXACTLY (law 7, P2), the checker's half: an entry's linked-effect
//! set holds what its code calls or spawns, and nothing it only passes on.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{Document, EffectName, FunctionDefinition, FunctionId, Name, parse_document};

use crate::{EntryEffectsRefusal, derive, entry_effects};

fn linked(text: &str, entry: &str) -> Result<BTreeSet<String>, EntryEffectsRefusal> {
    let document: Document = parse_document(text).unwrap_or_else(|error| panic!("{error}\n{text}"));
    let catalog: BTreeMap<FunctionId, FunctionDefinition> = BTreeMap::new();
    let graph = derive(&document, &catalog).unwrap_or_else(|refusal| panic!("{refusal:?}"));
    entry_effects(&document, &graph, &catalog, &Name::new(entry))
        .map(|effects| effects.keys().map(EffectName::to_string).collect())
}

fn names(effects: &[&str]) -> BTreeSet<String> {
    effects.iter().map(|effect| (*effect).to_owned()).collect()
}

/// The effects of the functions an entry calls and spawns are its own, as
/// are those of a closure it applies.
#[test]
fn calls_spawns_and_applied_closures_are_linked() {
    let text = r#"kernel 1
numbers float
effect a.run(input: Any) -> Any
effect b.run(input: Any) -> Any
effect c.run(input: Any) -> Any
entry start() -> Any

fn first() {
  let request = {n: 1}
  let got = perform a.run(request) as Any
  return got
}

fn second() {
  let request = {n: 2}
  let got = perform b.run(request) as Any
  return got
}

fn start() {
  let one = call first()
  let task = spawn call second()
  let done = join task
  let local = fn() {
    let request = {n: 3}
    let got = perform c.run(request) as Any
    return got
  }
  let three = apply local()
  return [one, done, three]
}

main {
  finish null
}
"#;
    assert_eq!(
        linked(text, "start").unwrap(),
        names(&["a.run", "b.run", "c.run"])
    );
}

/// A function an orchestrator only passes on, as a child's definition, is
/// a template: its effects are not the orchestrator's.
#[test]
fn a_passed_reference_links_nothing_of_its_function() {
    let text = r#"kernel 1
numbers float
effect processes.start(input: Any) -> Any
effect search.run(input: Any) -> Any
entry orchestrate() -> Any
entry worker() -> Any

fn worker() {
  let request = {q: "x"}
  let got = perform search.run(request) as Any
  return got
}

fn orchestrate() {
  let request = {definition: &worker}
  let handle = perform processes.start(request) as Any
  return handle
}

main {
  finish null
}
"#;
    assert_eq!(
        linked(text, "orchestrate").unwrap(),
        names(&["processes.start"])
    );
    assert_eq!(linked(text, "worker").unwrap(), names(&["search.run"]));
}

/// A call through a value the entry cannot certify, while it holds a
/// declared function it does not run, is refused, naming that function.
#[test]
fn an_uncertifiable_value_call_is_refused() {
    let text = r#"kernel 1
numbers float
effect search.run(input: Any) -> Any
entry start(f: Any) -> Any

fn worker() {
  let request = {q: "x"}
  let got = perform search.run(request) as Any
  return got
}

fn start(f) {
  let held = {definition: &worker}
  let got = apply f(held)
  return got
}

main {
  finish null
}
"#;
    assert!(matches!(
        linked(text, "start"),
        Err(EntryEffectsRefusal::ValueCall { template, .. }) if template == Name::new("worker")
    ));
}

/// P2: indirect callbacks of body and Both helpers cannot hide a held template.
#[test]
fn helper_callbacks_through_values_are_certified_or_refused() {
    for both in [false, true] {
        for argument in ["&worker", "g", "items"] {
            let body = if argument == "items" {
                "for callback in f { let got = apply callback() } return null"
            } else {
                "let got = apply f() return got"
            };
            let mut helper = lash_kernel_doc::parse_definition(&format!(
                "function helper.run(f: Any) -> Any\nkernel 1\ncharge 1\nbody {{ {body} }}\n"
            ))
            .unwrap();
            if both {
                let body = helper.body().unwrap().clone();
                helper.implementation = lash_kernel_doc::Implementation::Both(body);
            }
            let id = helper.identity();
            let catalog = BTreeMap::from([(id, helper)]);
            let text = format!(
                r#"kernel 1
numbers float
use helper.run = @{id}
effect search.run(input: Any) -> Any
entry start() -> Any
fn worker() {{ let got = perform search.run(null) as Any return got }}
fn start() {{
  let g = &worker
  let items = [g]
  let got = invoke helper.run({argument})
  return got
}}
main {{ finish null }}
"#
            );
            let spellings = if both {
                vec![
                    text.clone(),
                    text.replace("invoke helper.run(", "helper.run("),
                ]
            } else {
                vec![text]
            };
            for text in spellings {
                let document = parse_document(&text).unwrap();
                let graph = match derive(&document, &catalog) {
                    Ok(graph) => graph,
                    Err(refusal) if both => {
                        assert!(!refusal.errors.is_empty());
                        assert!(
                            refusal.errors.iter().all(|refused| matches!(
                                refused.reason,
                                crate::RefusalReason::Invalid(
                                    lash_kernel_doc::InvalidReason::NativeBodyMayWait { .. }
                                )
                            )),
                            "{refusal:?}"
                        );
                        continue;
                    }
                    Err(refusal) => panic!("{refusal:?}"),
                };
                let effects = entry_effects(&document, &graph, &catalog, &Name::new("start"));
                assert!(
                    matches!(&effects, Ok(effects) if effects.contains_key(&EffectName::new("search.run").unwrap()))
                        || matches!(effects, Err(EntryEffectsRefusal::ValueCall { template, .. }) if template == Name::new("worker")),
                    "helper callback {argument}, Both={both}, must link or refuse worker"
                );
            }
        }
    }
    // A helper that only carries a template does not call it, with either implementation.
    for both in [false, true] {
        let mut helper = lash_kernel_doc::parse_definition(
            "function helper.keep(f: Any) -> Any\nkernel 1\ncharge 1\nbody { return f }\n",
        )
        .unwrap();
        if both {
            helper.implementation =
                lash_kernel_doc::Implementation::Both(helper.body().unwrap().clone());
        }
        let id = helper.identity();
        let catalog = BTreeMap::from([(id, helper)]);
        let document = parse_document(&format!(
            r#"kernel 1
numbers float
use helper.keep = @{id}
effect search.run(input: Any) -> Any
entry start() -> Any
fn worker() {{ let got = perform search.run(null) as Any return got }}
fn start() {{ let held = invoke helper.keep(&worker) return held }}
main {{ finish null }}
"#
        ))
        .unwrap();
        let graph = derive(&document, &catalog).unwrap();
        assert!(
            entry_effects(&document, &graph, &catalog, &Name::new("start"))
                .unwrap()
                .is_empty()
        );
    }
}
