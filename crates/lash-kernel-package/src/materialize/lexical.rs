//! Ordinary lexical factories allocate cells before any captured reference is filled.
use super::*;
use lash_vocabulary::{Param, Signature, Type};

pub(super) fn definition(
    name: String,
    params: Vec<Param>,
    result: Type,
    block: Block,
    document: &Document,
    helpers: &BuildHelpers,
) -> Result<FunctionDefinition, MaterializeRefusal> {
    let names = document.function_names();
    let functions = Node::Block(&block)
        .functions_within()
        .into_iter()
        .filter_map(|id| {
            names
                .get(&id)
                .cloned()
                .or_else(|| helpers.header(&id).map(|h| h.name.clone()))
                .map(|name| (id, name))
        })
        .collect();
    let imports = Node::Block(&block)
        .imports_within()
        .into_iter()
        .map(|slot| (slot, document.imports[&slot].clone()))
        .collect();
    Ok(FunctionDefinition {
        kernel: KERNEL_VERSION,
        name: FunctionName::new(name).map_err(|e| MaterializeRefusal::Document {
            message: e.to_string(),
        })?,
        signature: Signature { params, result },
        errors: BTreeSet::new(),
        guard: None,
        implementation: Implementation::Guest {
            body: FunctionBody {
                functions,
                imports,
                block,
            },
            metering: Metering::Statements,
        },
        native_version: FIRST_NATIVE_VERSION,
    })
}

pub(super) fn environment(
    document: &mut Document,
    index: usize,
    node: &FunctionValue,
    raw: Expr,
    helpers: &BuildHelpers,
    fresh: &mut impl FnMut(String) -> Name,
) -> Result<FunctionId, MaterializeRefusal> {
    let mut params = vec![];
    let mut fill = vec![];
    let mut body = vec![];
    for (i, name) in node.captures.keys().enumerate() {
        body.push(binding(name.clone(), Expr::Literal(Literal::Null)));
        let input = fresh(format!("saved_input_{index}_{i}"));
        params.push(Param {
            name: input.clone(),
            ty: Type::Any,
            optional: false,
        });
        fill.push(Stmt::Assign {
            place: Place::Variable(name.clone()),
            value: Rhs::Expr(Expr::Variable(input)),
            order: AssignOrder::ValueFirst,
        });
    }
    fill.push(Stmt::Return {
        value: Expr::Literal(Literal::Null),
    });
    let setter = definition(
        format!("materialized.fill{index}"),
        params,
        Type::Null,
        fill,
        document,
        helpers,
    )?;
    let setter_id = setter.identity();
    document.functions.insert(setter_id, setter);
    body.push(Stmt::Return {
        value: Expr::Tuple(vec![
            raw,
            Expr::Closure(Box::new(Closure {
                function: setter_id,
                presentation: None,
            })),
        ]),
    });
    let factory = definition(
        format!("materialized.environment{index}"),
        vec![],
        Type::Any,
        body,
        document,
        helpers,
    )?;
    let factory_id = factory.identity();
    document.functions.insert(factory_id, factory);
    Ok(factory_id)
}

pub(super) fn member(name: &Name, index: i64) -> Expr {
    Expr::Member(Box::new(Member::Index {
        target: Expr::Variable(name.clone()),
        index: Expr::Literal(Literal::Int(index.into())),
    }))
}
