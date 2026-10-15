//! Author statements come from the same SWC and Ruff parsers as the dialects.
//! Function declarations, TS type declarations and braces are containers, not
//! executable statements. Loop declarations belong to their loop header.

use lash_kernel_doc::Span;
use serde_json::Value;
use swc_common::BytePos;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

pub(super) fn typescript(text: &str) -> Vec<Span> {
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            decorators: true,
            ..Default::default()
        }),
        Default::default(),
        StringInput::new(text, BytePos(0), BytePos(text.len() as u32)),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let module = parser.parse_module().expect("SWC workflow parse");
    assert!(
        parser.take_errors().is_empty(),
        "SWC recovered a syntax error"
    );
    // SWC's own tagged AST representation lets us walk expression-contained
    // arrow/function bodies too, without maintaining a second expression AST.
    let ast = serde_json::to_value(module).expect("SWC AST serialization");
    let mut spans = Vec::new();
    ts_nodes(&ast, "", "", &mut spans);
    spans.sort_by_key(|span| (span.start, span.end));
    spans
}

fn ts_nodes(node: &Value, parent: &str, field: &str, spans: &mut Vec<Span>) {
    match node {
        Value::Object(fields) => {
            let kind = fields
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let statement = (kind.ends_with("Statement")
                && !matches!(kind, "BlockStatement" | "LabeledStatement"))
                || (matches!(kind, "VariableDeclaration" | "UsingDeclaration")
                    && !(matches!(parent, "ForStatement" | "ForOfStatement" | "ForInStatement")
                        && matches!(field, "init" | "left")));
            if statement {
                let span = &fields["span"];
                spans.push(Span {
                    start: span["start"].as_u64().expect("SWC span start") as usize,
                    end: span["end"].as_u64().expect("SWC span end") as usize,
                });
            }
            for (name, child) in fields {
                ts_nodes(child, kind, name, spans);
            }
        }
        Value::Array(items) => {
            for item in items {
                ts_nodes(item, parent, field, spans);
            }
        }
        _ => {}
    }
}

pub(super) fn python(text: &str) -> Vec<Span> {
    use ruff_python_ast::visitor::{Visitor, walk_elif_else_clause, walk_stmt};
    use ruff_text_size::Ranged;

    struct Statements(Vec<Span>);
    impl<'a> Visitor<'a> for Statements {
        fn visit_stmt(&mut self, stmt: &'a ruff_python_ast::Stmt) {
            if !matches!(
                stmt,
                ruff_python_ast::Stmt::FunctionDef(_) | ruff_python_ast::Stmt::ClassDef(_)
            ) {
                let range = stmt.range();
                self.0.push(Span {
                    start: range.start().to_usize(),
                    end: range.end().to_usize(),
                });
            }
            walk_stmt(self, stmt);
        }
        fn visit_elif_else_clause(&mut self, clause: &'a ruff_python_ast::ElifElseClause) {
            if clause.test.is_some() {
                let range = clause.range();
                self.0.push(Span {
                    start: range.start().to_usize(),
                    end: range.end().to_usize(),
                });
            }
            walk_elif_else_clause(self, clause);
        }
    }
    let parsed = ruff_python_parser::parse_module(text).expect("Ruff workflow parse");
    let mut visitor = Statements(Vec::new());
    for stmt in &parsed.syntax().body {
        visitor.visit_stmt(stmt);
    }
    visitor.0.sort_by_key(|span| (span.start, span.end));
    visitor.0
}
