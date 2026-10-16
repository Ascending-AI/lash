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
    /// Read exported source against the base document it was printed from.
    pub decode: fn(&str, &k::Document, GeneratedSourceLimits) -> Result<k::Document, Diagnostic>,
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
    pub rule: String,
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
                let decoded = (dialect.decode)(&printed.source, &case.document, Default::default())
                    .map_err(|e| format!("{e:?}"))?;
                if decoded.canonical_bytes().map_err(|e| e.to_string())?
                    != case.document.canonical_bytes().map_err(|e| e.to_string())?
                {
                    return Err("canonical bytes differ".into());
                }
                if decoded.identity().map_err(|e| e.to_string())?
                    != case.document.identity().map_err(|e| e.to_string())?
                {
                    return Err("DocumentId differs".into());
                }
                let map = printed
                    .source_map
                    .as_ref()
                    .ok_or("printer omitted requested map")?;
                let mut expected = std::collections::BTreeSet::new();
                for (unit, root) in
                    std::iter::once((k::Unit::Main, k::Node::Block(&case.document.main))).chain(
                        case.document.functions.iter().map(|(name, function)| {
                            (
                                k::Unit::Function(name.clone()),
                                k::Node::Block(&function.body),
                            )
                        }),
                    )
                {
                    let mut stack = vec![(k::Site::new(unit, vec![]), root)];
                    while let Some((site, node)) = stack.pop() {
                        for (index, child) in node.children().into_iter().enumerate() {
                            stack.push((site.child(index as u32), child));
                        }
                        expected.insert(site);
                    }
                }
                let actual: std::collections::BTreeSet<_> =
                    map.sites.iter().map(|entry| entry.site.clone()).collect();
                if actual != expected || actual.len() != map.sites.len() {
                    return Err("map does not cover every real kernel Site exactly once".into());
                }
                // The environment stays in the base document: no header,
                // directive, table or kernel form may stand outside the code
                // of a node, where no Site could own it.
                let mut scaffolding = printed.source.as_bytes().to_vec();
                for mapped in &map.sites {
                    for range in &mapped.ranges {
                        scaffolding[range.start..range.end].fill(b' ');
                    }
                }
                let scaffolding = std::str::from_utf8(&scaffolding).map_err(|e| e.to_string())?;
                if let Some(form) = environment_form(scaffolding) {
                    return Err(format!(
                        "`{form}` outside any node: exported source carries a header, directive or table"
                    ));
                }
                for mapped in &map.sites {
                    if mapped.ranges.is_empty()
                        && !matches!(site_node(&case.document, &mapped.site), Some(k::Node::Block(body)) if body.is_empty() && mapped.implicit.is_some_and(|offset| printed.source.is_char_boundary(offset)))
                    {
                        return Err("executable node has no source range".into());
                    }
                    let node = site_node(&case.document, &mapped.site)
                        .ok_or("map names a nonexistent node")?;
                    for range in &mapped.ranges {
                        if range.start >= range.end {
                            return Err("mapped range is empty or reversed".into());
                        }
                        let source = printed
                            .source
                            .get(range.start..range.end)
                            .ok_or("invalid mapped UTF-8 range")?;
                        // Remove descendant ranges before lexing: each spelling belongs
                        // to its innermost real Site, and quoted data is never syntax.
                        let mut owned = printed.source.as_bytes()[range.start..range.end].to_vec();
                        for child in &map.sites {
                            if child.site.unit == mapped.site.unit
                                && child.site.path.len() > mapped.site.path.len()
                                && child.site.path.starts_with(&mapped.site.path)
                            {
                                for child_range in &child.ranges {
                                    let start = child_range.start.max(range.start);
                                    let end = child_range.end.min(range.end);
                                    if start < end {
                                        owned[start - range.start..end - range.start].fill(b' ');
                                    }
                                }
                            }
                        }
                        let owned = std::str::from_utf8(&owned).map_err(|e| e.to_string())?;
                        let mut rules = fallback_rules(owned);
                        if rules.is_empty() && !owned.trim().is_empty() {
                            rules.extend((dialect.generic)(node, owned));
                        }
                        for detected in rules {
                            let rule = if detected == "kernel-form" {
                                (dialect.generic)(node, source).unwrap_or(detected)
                            } else {
                                detected
                            };
                            let node_kind = node_kind(node);
                            let allowed = dialect.unavoidable.iter().find(|allowed| {
                                allowed.rule == rule
                                    && allowed.node_kind == node_kind
                                    && !allowed.reason.is_empty()
                            });
                            let reason = allowed
                                .map_or("no documented unavoidable reason", |allowed| {
                                    allowed.reason
                                });
                            report.generic_forms.push(Occurrence {
                                dialect: dialect.name.into(),
                                case: case.name,
                                site: mapped.site.clone(),
                                node_kind,
                                rule: rule.into(),
                                reason,
                            });
                            if allowed.is_none() {
                                report.failures.push(format!(
                                    "{} / {} / {}: {node_kind} {rule}: {reason}: {owned}",
                                    dialect.name, case.name, mapped.site
                                ));
                            }
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

/// Outside the code of its nodes, exported source holds only function
/// declarations: names, keywords and their parameter lists. Data, member
/// access, statements or any fallback there is an environment the source
/// should not carry.
fn environment_form(scaffolding: &str) -> Option<String> {
    if let Some(rule) = fallback_rules(scaffolding).first() {
        return Some((*rule).to_owned());
    }
    scaffolding
        .char_indices()
        .find(|(_, ch)| {
            !(ch.is_whitespace()
                || ch.is_alphanumeric()
                || matches!(ch, '_' | '(' | ')' | ',' | ':'))
        })
        .map(|(start, _)| scaffolding[start..].chars().take(40).collect())
}

/// This lexer belongs to the harness, so a dialect cannot self-grade away forms.
fn fallback_rules(source: &str) -> Vec<&str> {
    let mut tokens = vec![];
    let mut chars = source.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        if matches!(ch, '\'' | '"' | '`') {
            let mut escaped = false;
            for (_, next) in chars.by_ref() {
                if escaped {
                    escaped = false;
                } else if next == '\\' {
                    escaped = true;
                } else if next == ch {
                    break;
                }
            }
        } else if ch == '#' {
            for (_, next) in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
        } else if ch.is_alphanumeric() || ch == '_' {
            let mut end = start + ch.len_utf8();
            while let Some((index, next)) = chars.peek().copied() {
                if !next.is_alphanumeric() && next != '_' {
                    break;
                }
                chars.next();
                end = index + next.len_utf8();
            }
            tokens.push(&source[start..end]);
        } else if !ch.is_whitespace() {
            tokens.push(&source[start..start + ch.len_utf8()]);
        }
    }
    let mut rules = vec![];
    for (i, token) in tokens.iter().enumerate() {
        let next = tokens.get(i + 1).copied();
        let rule = match *token {
            "k" if next == Some(".") => Some("kernel-form"),
            token if token.starts_with("helper_") && next == Some("(") => Some(token),
            "spawn" | "sleep" | "join" | "cancel" | "yieldTask" if next == Some("(") => {
                Some("task-action")
            }
            "projection" if next == Some(".") && tokens.get(i + 2) == Some(&"read") => {
                Some("projection-read")
            }
            "Promise"
                if next == Some(".")
                    && tokens.get(i + 2).is_some_and(|name| {
                        matches!(*name, "all" | "allSettled" | "race" | "any")
                    }) =>
            {
                Some("task-action")
            }
            "-" if next == Some("(") => Some("unary-identity"),
            "record" if next == Some("(") => Some("record-kind"),
            "returned" if next == Some("(") => Some("return-kind"),
            "finish" if next == Some("(") => Some("finish-kind"),
            "failure" if next == Some("(") => Some("failure-kind"),
            "declared" if next == Some(".") => Some("declared-callee"),
            "Final" | "reassigned" => Some("binding-kind"),
            "KernelType" => Some("result-type"),
            "Object" if next == Some(".") && tokens.get(i + 2) == Some(&"fromEntries") => {
                Some("record-order")
            }
            _ => None,
        };
        if let Some(rule) = rule {
            rules.push(rule);
        }
    }
    rules
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
            k::Expr::Act(_) => "Act",
        },
    }
}

/// A real agent cell (FIG-6272): a three-tool deep-research entry with typed
/// tool signatures, as a host stored it. Every dialect must print it as
/// plain source and read it back to the same identity.
pub fn deep_research() -> Result<k::Document, Diagnostic> {
    k::Document::from_json(DEEP_RESEARCH).map_err(|error| corpus_error(error.to_string()))
}

const DEEP_RESEARCH: &str = r#"{"manifest":{"kernel":1,"effects":{"display.show_message":{"params":[{"name":"input","ty":{"record":{"fields":[{"name":"text","ty":"text"}]}}}],"result":"null"},"research.search":{"params":[{"name":"input","ty":{"record":{"fields":[{"name":"max_sources","ty":"int"},{"name":"topic","ty":"text"}]}}}],"result":{"record":{"fields":[{"name":"sources","ty":{"list":{"record":{"fields":[{"name":"excerpt","ty":"text"},{"name":"id","ty":"text"},{"name":"title","ty":"text"},{"name":"url","ty":"text"}],"rest":"any"}}}},{"name":"topic","ty":"text"}],"rest":"any"}}},"research.synthesize":{"params":[{"name":"input","ty":{"record":{"fields":[{"name":"sources","ty":{"list":{"record":{"fields":[{"name":"excerpt","ty":"text"},{"name":"id","ty":"text"},{"name":"title","ty":"text"},{"name":"url","ty":"text"}]}}}},{"name":"topic","ty":"text"}]}}}],"result":{"record":{"fields":[{"name":"citations","ty":{"list":{"record":{"fields":[{"name":"source_id","ty":"text"},{"name":"url","ty":"text"}],"rest":"any"}}}},{"name":"source_count","ty":"int"},{"name":"summary","ty":"text"},{"name":"title","ty":"text"}],"rest":"any"}}}},"numbers":"float"},"functions":{"deep_research":{"body":[{"let":{"name":"search_input","value":{"expr":{"record":[{"field":"topic","value":{"literal":{"text":"Reducing customer onboarding drop-off"}}},{"field":"max_sources","value":{"literal":{"int":"3"}}}]}}}},{"let":{"name":"evidence","value":{"action":{"perform":{"effect":"research.search","args":[{"variable":"search_input"}],"result":"any"}}}}},{"let":{"name":"synthesis_input","value":{"expr":{"record":[{"field":"topic","value":{"member":{"field":{"target":{"variable":"evidence"},"field":"topic"}}}},{"field":"sources","value":{"member":{"field":{"target":{"variable":"evidence"},"field":"sources"}}}}]}}}},{"let":{"name":"report","value":{"action":{"perform":{"effect":"research.synthesize","args":[{"variable":"synthesis_input"}],"result":"any"}}}}},{"let":{"name":"display_input","value":{"expr":{"record":[{"field":"text","value":{"member":{"field":{"target":{"variable":"report"},"field":"summary"}}}}]}}}},{"do":{"action":{"perform":{"effect":"display.show_message","args":[{"variable":"display_input"}],"result":"any"}}}},{"return":{"value":{"variable":"report"}}}]}},"entries":{"deep_research":{"result":"any"}},"main":[{"finish":{"value":{"literal":"null"}}}]}"#;

/// Seven realistic shapes shared by all dialects. Helper identities are
/// deliberately foreign: decoding and readability require no installed library.
pub fn corpus() -> Result<Vec<Case>, Diagnostic> {
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
 if n { let next = call recurse(false)
 return next } else { return n }
}
main { let answer = call recurse(true)
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
 let answer = perform tools.lookup(query) as Any
 let records = [answer]
 for item in records { print item.title }
 finish answer
}"#,
        ),
    ];
    programs
        .into_iter()
        .map(|(name, program)| {
            let mut document = k::parse_document(&format!("kernel 1\nnumbers float\n{program}\n"))
                .map_err(|error| corpus_error(error.to_string()))?;
            if name == "recursion_with_helpers" {
                let id = k::FunctionId::from_bytes([23; 32]);
                document.manifest.functions.insert(
                    id,
                    k::FunctionName::new("foreign.identity")
                        .map_err(|error| corpus_error(error.to_string()))?,
                );
                document
                    .functions
                    .get_mut(&k::Name::new("recurse"))
                    .ok_or_else(|| {
                        corpus_error("recursion fixture is missing its declaration".into())
                    })?
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
            Ok(Case { name, document })
        })
        .collect()
}

fn corpus_error(message: String) -> Diagnostic {
    Diagnostic {
        code: "GENERATED_READABILITY_CORPUS".into(),
        message,
        span: None,
        kind: crate::DiagnosticKind::ProgramDefect,
        repairs: vec![],
    }
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
    struct Spelling(&'static str);
    impl Printer for Spelling {
        fn print(
            &self,
            document: &k::Document,
            options: PrintOptions,
        ) -> Result<crate::PrintedSource, PrintError> {
            let mut printed = Witness.print(document, options)?;
            let old = printed.source.len();
            printed.source = printed.source.strip_suffix("generic").unwrap().to_owned() + self.0;
            printed.source_map.as_mut().unwrap().sites[0].ranges[0].end = printed.source.len();
            assert!(old > 0);
            Ok(printed)
        }
    }
    impl Printer for Witness {
        fn print(
            &self,
            document: &k::Document,
            options: PrintOptions,
        ) -> Result<crate::PrintedSource, PrintError> {
            let mut writer = SourceWriter::new(options);
            writer.write("main\n");
            writer.begin_node(Some(&k::Unit::Main));
            writer.write("generic");
            writer.end_node();
            writer.finish(document, SourceFormat::TypeScriptV1)
        }
    }
    fn decode(
        _: &str,
        base: &k::Document,
        _: GeneratedSourceLimits,
    ) -> Result<k::Document, Diagnostic> {
        Ok(base.clone())
    }
    fn mutated_decode(
        source: &str,
        base: &k::Document,
        limits: GeneratedSourceLimits,
    ) -> Result<k::Document, Diagnostic> {
        let mut document = decode(source, base, limits)?;
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
        let second = Dialect {
            name: "second",
            printer: &Witness,
            decode,
            generic,
            unavoidable: &allowed,
        };
        let both = check(&cases, &[dialect, second]);
        both.assert_passed();
        assert_eq!(both.documents, 2);
        assert_eq!(both.generic_forms.len(), 2);
        let incomplete = [Case {
            name: "incomplete",
            document: k::Document::new(
                k::NumberPolicy::Float,
                vec![k::Stmt::Finish {
                    value: k::Expr::Literal(k::Literal::Null),
                }],
            ),
        }];
        let mut dialect = Dialect {
            name: "witness",
            printer: &Witness,
            decode,
            generic,
            unavoidable: &allowed,
        };
        assert!(
            check(&incomplete, std::slice::from_ref(&dialect)).failures[0]
                .contains("map does not cover every real kernel Site")
        );
        dialect.decode = mutated_decode;
        let unequal = check(&cases, &[dialect]);
        assert_eq!(
            unequal.failures,
            ["witness / empty: canonical bytes differ"]
        );
    }
    /// FIG-6272: the environment stays in the base document. A header,
    /// directive or table outside every node fails, however it is spelled.
    struct Header(&'static str);
    impl Printer for Header {
        fn print(
            &self,
            document: &k::Document,
            options: PrintOptions,
        ) -> Result<crate::PrintedSource, PrintError> {
            let mut writer = SourceWriter::new(options);
            writer.write(self.0);
            writer.write("\nmain\n");
            writer.begin_node(Some(&k::Unit::Main));
            writer.write("generic");
            writer.end_node();
            writer.finish(document, SourceFormat::TypeScriptV1)
        }
    }
    #[test]
    fn headers_directives_and_tables_outside_nodes_fail() {
        let cases = [Case {
            name: "header",
            document: k::Document::new(k::NumberPolicy::Float, vec![]),
        }];
        let allowed = [Unavoidable {
            rule: "witness",
            node_kind: "Block",
            reason: "test witness has no native spelling",
        }];
        for header in [
            r#"k.document("lash.generated.typescript", "1", {}, {}, [], [], [])"#,
            r#"{"kernel": 1, "numbers": "float"}"#,
            "declare const tools: any;",
            "# lash: numbers=float",
            "\"use kernel\"",
            "helper_add = 1",
        ] {
            let printer = Header(header);
            let report = check(
                &cases,
                &[Dialect {
                    name: "witness",
                    printer: &printer,
                    decode,
                    generic,
                    unavoidable: &allowed,
                }],
            );
            assert!(
                report
                    .failures
                    .iter()
                    .any(|failure| failure.contains("outside any node")),
                "{header}: {:?}",
                report.failures
            );
        }
    }
    #[test]
    fn detectors_cannot_hide_helpers_or_pseudo_intrinsics() {
        let cases = [Case {
            name: "fallback",
            document: k::Document::new(k::NumberPolicy::Float, vec![]),
        }];
        for source in [
            "k.random()",
            "spawn(worker, [])",
            "await sleep(1)",
            "await join(task)",
            "await cancel(task)",
            "await yieldTask()",
            "await Promise.all(tasks)",
            "projection.read(handle, request)",
            "-(7)",
            "helper_num_add(1, 2)",
            "record({})",
            "return returned(None)",
            "return finish(None)",
            "raise failure(None)",
            "declared.worker",
            "x: Final = 1",
            "x: reassigned = 1",
            "x as KernelType<\"any\">",
            "Object.fromEntries([])",
        ] {
            let printer = Spelling(source);
            let report = check(
                &cases,
                &[Dialect {
                    name: "witness",
                    printer: &printer,
                    decode,
                    generic: |_, _| None,
                    unavoidable: &[],
                }],
            );
            assert!(!report.generic_forms.is_empty(), "detector missed {source}");
            assert!(!report.failures.is_empty());
        }
        let printer = Spelling("\"k.random() helper_add() record({})\"");
        let report = check(
            &cases,
            &[Dialect {
                name: "witness",
                printer: &printer,
                decode,
                generic: |_, _| None,
                unavoidable: &[],
            }],
        );
        report.assert_passed();
        assert!(report.generic_forms.is_empty());
    }
}
