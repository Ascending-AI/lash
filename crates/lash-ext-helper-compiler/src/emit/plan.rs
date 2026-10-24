//! Assemble one emitted body, its plan descriptor and generated template.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use lash_kernel_doc::{FunctionDefinition, Node};

use lash_vocabulary::{FunctionId, Name};

use super::{Emitter, Plan, Unsupported, Var};
use crate::library::Library;

/// Compiles `definition`'s body, whose identity is `function`, to the
/// plan named `ident`: a closed body when `segment` is false, whose every
/// callee must be in `answering` or have no body, and a resumable segment
/// otherwise.
///
/// # Errors
///
/// The first form the compiler does not perform as the body does.
pub fn emit(
    library: &Library,
    answering: &BTreeSet<FunctionId>,
    function: FunctionId,
    definition: &FunctionDefinition,
    ident: &str,
    segment: bool,
) -> Result<Plan, Unsupported> {
    let body = definition
        .body()
        .ok_or_else(|| Unsupported("has no body".into()))?;
    let params: Vec<Name> = definition
        .signature
        .params
        .iter()
        .map(|param| param.name.clone())
        .collect();
    let mut emitter = Emitter {
        library,
        answering,
        function,
        segment,
        statements: matches!(definition.metering(), lash_kernel_doc::Metering::Statements),
        callees: Vec::new(),
        literals: Vec::new(),
        fields: Vec::new(),
        records: Vec::new(),
        slots: Vec::new(),
        closures: Vec::new(),
        stmt_paths: BTreeMap::new(),
        expr_paths: BTreeMap::new(),
        protected: Vec::new(),
        adopted: BTreeMap::new(),
        scopes: Vec::new(),
        loops: Vec::new(),
        out: String::new(),
        indent: 1,
        used: 0,
        path: Vec::new(),
        live: Vec::new(),
        stmt_depth: 0,
        guard: segment,
        sites: Vec::new(),
        enclosing: Vec::new(),
        fors: 0,
        whiles: 0,
        tests: 0,
    };
    let mut pending = vec![(Node::Block(&body.block), Vec::new())];
    while let Some((node, path)) = pending.pop() {
        match node {
            Node::Stmt(stmt) => {
                emitter
                    .stmt_paths
                    .insert(std::ptr::from_ref(stmt), path.clone());
            }
            Node::Expr(expr) => {
                emitter
                    .expr_paths
                    .insert(std::ptr::from_ref(expr), path.clone());
            }
            _ => {}
        }
        for (index, child) in node.children().into_iter().enumerate() {
            let mut child_path = path.clone();
            child_path.push(index as u32);
            pending.push((child, child_path));
        }
    }
    if segment {
        emitter.line("let mut r = entry;");
    }
    let mut bound = Vec::new();
    for (index, name) in params.iter().enumerate() {
        // A parameter is declared at the body.
        let slot = emitter.named_slot(name, Vec::new());
        emitter.adopted.insert(index, slot);
        emitter.op(&format!("ctx.adopt({index}, {slot})?;"));
        bound.push((name.clone(), Var::Param(index)));
    }
    emitter.block(&body.block, bound, !segment)?;
    if segment {
        if emitter.sites.is_empty() {
            return Err(Unsupported("has no callback site".into()));
        }
        // The body's end returns null (`K-FN-003`); a resume that found
        // no site ends here too.
        emitter.line("if r != 0 {");
        emitter.line("    return Err(ctx.lost(r));");
        emitter.line("}");
        emitter.line("ctx.begin()?;");
        emitter.line("Ok(Value::Null)");
    }
    let registers = emitter.used.max(1);
    let mut item = String::new();
    let _ = writeln!(
        item,
        "const {}: p::HelperPlan = p::HelperPlan {{",
        crate::plan_const(ident)
    );
    let _ = writeln!(item, "    name: {:?},", definition.name.as_str());
    let _ = writeln!(
        item,
        "    function: p::function({:?}),",
        function.to_string()
    );
    item.push_str("    callees: &[");
    for callee in &emitter.callees {
        let _ = write!(item, "\n        p::function({:?}),", callee.to_string());
    }
    item.push_str(if emitter.callees.is_empty() {
        "],\n"
    } else {
        "\n    ],\n"
    });
    item.push_str("    literals: &[");
    for literal in &emitter.literals {
        let _ = write!(item, "\n        {literal},");
    }
    item.push_str(if emitter.literals.is_empty() {
        "],\n"
    } else {
        "\n    ],\n"
    });
    item.push_str("    fields: &[");
    for field in &emitter.fields {
        let _ = write!(item, "{field:?}, ");
    }
    item.push_str("],\n");
    item.push_str("    records: &[");
    for record in &emitter.records {
        let fields: Vec<String> = record.iter().map(u32::to_string).collect();
        let _ = write!(item, "&[{}], ", fields.join(", "));
    }
    item.push_str("],\n");
    let _ = writeln!(item, "    registers: {registers},");
    item.push_str("    slots: &[\n");
    for (name, path) in &emitter.slots {
        let _ = writeln!(
            item,
            "        p::PlanLocal {{ name: {name:?}, path: &{path:?} }},"
        );
    }
    item.push_str("    ],\n    closures: &[\n");
    for path in &emitter.closures {
        let _ = writeln!(item, "        &{path:?},");
    }
    item.push_str("    ],\n");
    item.push_str("    sites: &[");
    for site in &emitter.sites {
        let _ = write!(item, "\n        {site},");
    }
    item.push_str(if emitter.sites.is_empty() {
        "],\n"
    } else {
        "\n    ],\n"
    });
    let _ = writeln!(item, "    run: {ident},");
    let template = crate::template::emit(library, definition, ident);
    let _ = writeln!(
        item,
        "    template: {},",
        template
            .as_ref()
            .map_or("None", |(descriptor, _)| descriptor)
    );
    item.push_str("};\n");
    let mut code = String::new();
    let _ = writeln!(code, "/// `{}`.", definition.name);
    let entry = if segment { "entry" } else { "_entry" };
    let _ = writeln!(
        code,
        "fn {ident}(ctx: &mut dyn p::PlanContext, {entry}: u32) -> Result<Value, p::PlanExit> {{"
    );
    code.push_str(&emitter.out);
    code.push_str("}\n");
    if let Some((_, template)) = template {
        code.push_str(&template);
    }
    Ok(Plan {
        item,
        code,
        sites: emitter.sites.len(),
    })
}
