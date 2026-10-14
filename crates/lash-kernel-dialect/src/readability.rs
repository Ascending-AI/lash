//! G-SOURCE-READABLE: one kernel corpus, registered dialect codecs, exact bytes.
//! This acceptance harness is separate from production printing. A dialect
//! detector classifies mapped nodes; an allowlist must name the precise rule,
//! node kind and documented reason, rather than exempting a whole program.
use crate::{Diagnostic, GeneratedSourceLimits, PrintOptions, Printer};
use lash_kernel_doc as k;

pub struct Case {
    pub name: &'static str,
    pub document: k::Document,
}

pub struct Unavoidable {
    pub rule: &'static str,
    pub node_kind: &'static str,
    pub reason: &'static str,
}

pub struct Dialect<'a> {
    pub name: &'a str,
    pub printer: &'a dyn Printer,
    pub decode: fn(&str, GeneratedSourceLimits) -> Result<k::Document, Diagnostic>,
    /// Return a documented generic rule for this node's final source range.
    /// Do not scan quoted data or count child occurrences again in a parent.
    pub generic: fn(k::Node<'_>, &str) -> Option<&'static str>,
    pub unavoidable: &'a [Unavoidable],
}

#[derive(Debug)]
pub struct Occurrence {
    pub dialect: String,
    pub case: &'static str,
    pub site: k::Site,
    pub node_kind: &'static str,
    pub rule: &'static str,
    pub reason: &'static str,
}

#[derive(Debug, Default)]
pub struct Report {
    pub documents: usize,
    pub generic_forms: Vec<Occurrence>,
    pub failures: Vec<String>,
}
impl Report {
    pub fn assert_passed(&self) {
        assert!(
            self.documents > 0,
            "readability registration/corpus is empty"
        );
        assert!(self.failures.is_empty(), "{}", self.failures.join("\n"));
    }
}

/// Print every case with every registered codec. A printer refusal, missing
/// map, byte mismatch, undecodable result or undocumented generic form fails.
/// Every detected form is reported with its real kernel Site and reason.
pub fn check(cases: &[Case], dialects: &[Dialect<'_>]) -> Report {
    let mut report = Report::default();
    for dialect in dialects {
        for case in cases {
            report.documents += 1;
            let result = (|| -> Result<(), String> {
                let printed = dialect
                    .printer
                    .print(
                        &case.document,
                        PrintOptions {
                            source_map: true,
                            ..Default::default()
                        },
                    )
                    .map_err(|e| e.to_string())?;
                let decoded = (dialect.decode)(&printed.source, Default::default())
                    .map_err(|e| format!("{e:?}"))?;
                if decoded.canonical_bytes().map_err(|e| e.to_string())?
                    != case.document.canonical_bytes().map_err(|e| e.to_string())?
                {
                    return Err("canonical bytes differ".into());
                }
                let map = printed
                    .source_map
                    .as_ref()
                    .ok_or("printer omitted requested map")?;
                for mapped in &map.sites {
                    let node = site_node(&case.document, &mapped.site)
                        .ok_or("map names a nonexistent node")?;
                    for range in &mapped.ranges {
                        let source = printed
                            .source
                            .get(range.start..range.end)
                            .ok_or("invalid mapped UTF-8 range")?;
                        let Some(rule) = (dialect.generic)(node, source) else {
                            continue;
                        };
                        let node_kind = node_kind(node);
                        let allowed = dialect.unavoidable.iter().find(|allowed| {
                            allowed.rule == rule
                                && allowed.node_kind == node_kind
                                && !allowed.reason.is_empty()
                        });
                        let reason = allowed
                            .map_or("no documented unavoidable reason", |allowed| allowed.reason);
                        report.generic_forms.push(Occurrence {
                            dialect: dialect.name.into(),
                            case: case.name,
                            site: mapped.site.clone(),
                            node_kind,
                            rule,
                            reason,
                        });
                        if allowed.is_none() {
                            report.failures.push(format!(
                                "{} / {} / {}: {node_kind} {rule}: {reason}",
                                dialect.name, case.name, mapped.site
                            ));
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                report
                    .failures
                    .push(format!("{} / {}: {error}", dialect.name, case.name));
            }
        }
    }
    report
}

pub fn node_kind(node: k::Node<'_>) -> &'static str {
    match node {
        k::Node::Block(_) => "Block",
        k::Node::Stmt(stmt) => match stmt {
            k::Stmt::Let { .. } => "Let",
            k::Stmt::Assign { .. } => "Assign",
            k::Stmt::Remove { .. } => "Remove",
            k::Stmt::Do { .. } => "Do",
            k::Stmt::If { .. } => "If",
            k::Stmt::For { .. } => "For",
            k::Stmt::While { .. } => "While",
            k::Stmt::Break => "Break",
            k::Stmt::Continue => "Continue",
            k::Stmt::Return { .. } => "Return",
            k::Stmt::Try(_) => "Try",
            k::Stmt::Throw { .. } => "Throw",
            k::Stmt::Print { .. } => "Print",
            k::Stmt::Finish { .. } => "Finish",
            k::Stmt::Fail { .. } => "Fail",
        },
        k::Node::Action(action) => match action {
            k::Action::Call { .. } => "Call",
            k::Action::Perform { .. } => "Perform",
            k::Action::Sleep { .. } => "Sleep",
            k::Action::Join { .. } => "Join",
            k::Action::JoinMany { .. } => "JoinMany",
            k::Action::Yield => "Yield",
            k::Action::Spawn { .. } => "Spawn",
            k::Action::Cancel { .. } => "Cancel",
        },
        k::Node::Expr(expr) => match expr {
            k::Expr::Literal(_) => "Literal",
            k::Expr::Variable(_) => "Variable",
            k::Expr::Tuple(_) => "Tuple",
            k::Expr::List(_) => "List",
            k::Expr::Map(_) => "Map",
            k::Expr::Set(_) => "Set",
            k::Expr::Record(_) => "Record",
            k::Expr::Member(_) => "Member",
            k::Expr::Closure(_) => "Closure",
            k::Expr::Call { .. } => "ExpressionCall",
            k::Expr::Clock => "Clock",
            k::Expr::Random => "Random",
            k::Expr::Read(_) => "Read",
        },
    }
}

/// Seven realistic shapes shared by all dialects. Helper identities are
/// deliberately foreign: decoding and readability require no installed library.
pub fn corpus() -> Vec<Case> {
    let programs = [
        (
            "three_effect_workflow",
            r#"effect research.search(input: Any) -> Any
effect research.synthesize(input: Any) -> Any
effect display.show_message(input: Any) -> Any
entry deep_research() -> Any
fn deep_research() {
 let search_input = {topic: "Reducing customer onboarding drop-off", max_sources: 3}
 let evidence = perform research.search(search_input) as Any
 let synthesis_input = {topic: evidence.topic, sources: evidence.sources}
 let synthesis = perform research.synthesize(synthesis_input) as Any
 let message = {text: synthesis.summary}
 do perform display.show_message(message) as Any
 return synthesis
}
main { finish null }"#,
        ),
        (
            "recursion_with_helpers",
            r#"entry recurse(n: Any) -> Any
fn recurse(n) {
 if n { let next = invoke recurse(false)
 return next } else { return n }
}
main { let answer = invoke recurse(true)
finish answer }"#,
        ),
        (
            "nested_control_flow",
            r#"main {
 let ready = true
 while ready {
  for item in [true, false] {
   if item { continue } else { break }
  }
  set ready = false
 }
 finish ready
}"#,
        ),
        (
            "loops_over_records",
            r#"entry collect() -> Any
fn collect() {
 let rows = [{title: "first", active: true}, {title: "second", active: false}]
 for row in rows { if row.active { print row.title } }
 return rows
}
main { finish null }"#,
        ),
        (
            "error_handling",
            r#"main {
 try { throw "bad input" } catch error { print error } finally { print "done" }
 finish null
}"#,
        ),
        (
            "multiple_entries",
            r#"entry first() -> Any
entry second(input: Any) -> Any
fn first() { return {title: "first"} }
fn second(input) { return input.title }
main { finish null }"#,
        ),
        (
            "rlm_tool_cell",
            r#"effect tools.lookup(query: Any) -> Any
main {
 let query = "find source"
 let result = perform tools.lookup(query) as Any
 let records = [result]
 for item in records { print item.title }
 finish result
}"#,
        ),
    ];
    programs
        .into_iter()
        .map(|(name, program)| {
            let mut document = k::parse_document(&format!("kernel 1\nnumbers float\n{program}\n"))
                .expect("readability corpus is valid kernel text");
            if name == "recursion_with_helpers" {
                let id = k::FunctionId::from_bytes([23; 32]);
                document
                    .manifest
                    .functions
                    .insert(id, k::FunctionName::new("foreign.identity").unwrap());
                document
                    .functions
                    .get_mut(&k::Name::new("recurse"))
                    .unwrap()
                    .body
                    .insert(
                        0,
                        k::Stmt::Let {
                            name: "checked".into(),
                            value: k::Rhs::Expr(k::Expr::Call {
                                function: id,
                                args: vec![k::Expr::Variable("n".into())],
                            }),
                        },
                    );
            }
            Case { name, document }
        })
        .collect()
}

fn site_node<'a>(document: &'a k::Document, site: &k::Site) -> Option<k::Node<'a>> {
    let mut node = match &site.unit {
        k::Unit::Main => k::Node::Block(&document.main),
        k::Unit::Function(name) => k::Node::Block(&document.functions.get(name)?.body),
        k::Unit::Library(_) => return None,
    };
    for index in &site.path {
        node = *node.children().get(*index as usize)?;
    }
    Some(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PrintError, SourceFormat, SourceWriter};

    struct Witness;
    impl Printer for Witness {
        fn print(
            &self,
            document: &k::Document,
            options: PrintOptions,
        ) -> Result<crate::PrintedSource, PrintError> {
            let mut writer = SourceWriter::new(options);
            writer.json(document);
            writer.write("\n");
            writer.begin_node(Some(&k::Unit::Main));
            writer.write("generic");
            writer.end_node();
            writer.finish(document, SourceFormat::TypeScriptV1)
        }
    }
    fn decode(source: &str, _: GeneratedSourceLimits) -> Result<k::Document, Diagnostic> {
        Ok(k::Document::from_json(source.split_once('\n').unwrap().0).unwrap())
    }
    fn mutated_decode(
        source: &str,
        limits: GeneratedSourceLimits,
    ) -> Result<k::Document, Diagnostic> {
        let mut document = decode(source, limits)?;
        document.manifest.numbers = k::NumberPolicy::BySpelling;
        Ok(document)
    }
    fn generic(_: k::Node<'_>, source: &str) -> Option<&'static str> {
        (source == "generic").then_some("witness")
    }
    #[test]
    fn acceptance_requires_exact_bytes_and_a_named_unavoidable_reason() {
        let cases = [Case {
            name: "empty",
            document: k::Document::new(k::NumberPolicy::Float, vec![]),
        }];
        let allowed = [Unavoidable {
            rule: "witness",
            node_kind: "Block",
            reason: "test witness has no native spelling",
        }];
        let mut dialect = Dialect {
            name: "witness",
            printer: &Witness,
            decode,
            generic,
            unavoidable: &[],
        };
        let rejected = check(&cases, std::slice::from_ref(&dialect));
        assert_eq!(rejected.generic_forms.len(), 1);
        assert!(rejected.failures[0].contains("Block witness: no documented unavoidable reason"));
        dialect.unavoidable = &allowed;
        check(&cases, std::slice::from_ref(&dialect)).assert_passed();
        dialect.decode = mutated_decode;
        let unequal = check(&cases, &[dialect]);
        assert_eq!(
            unequal.failures,
            ["witness / empty: canonical bytes differ"]
        );
    }
}
