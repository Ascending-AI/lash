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
) -> Result<Expr, MaterializeRefusal> {
    let ts = matches!(node.convention, StartConvention::TsArgsList { .. });
    let abi = if ts {
        source::Abi::TypeScript
    } else if node
        .written_signature
        .as_ref()
        .is_some_and(|written| written.language == SourceLanguage::Python)
    {
        source::Abi::Python
    } else {
        source::Abi::Kernel
    };
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
    let setter_name = fresh(format!("fill_environment_{index}"));
    let setter = source::closure(
        document,
        setter_name.to_string(),
        params,
        fill,
        abi,
        helpers,
        fresh,
    )?;
    body.push(binding(setter_name.clone(), setter));
    let raw_name = node
        .token
        .as_ref()
        .and_then(|token| token.get(2))
        .and_then(|v| {
            if let Datum::Text(name) = v {
                Some(Name::new(name))
            } else {
                None
            }
        })
        .or_else(|| {
            node.signature_origin
                .as_ref()
                .and_then(|origin| origin.name.as_ref())
                .map(Name::new)
        })
        .unwrap_or_else(|| fresh(format!("make_callable_{index}")));
    let raw = match (&node.representation, &node.token) {
        (FunctionRepresentation::Bare, None) => raw,
        (FunctionRepresentation::Token { callable, .. }, Some(token)) if ts => {
            let mut values = Values::new(&[], document.numbers, helpers);
            let mut parts = Vec::new();
            for (i, datum) in token.iter().enumerate() {
                parts.push(if i == *callable as usize {
                    raw.clone()
                } else {
                    values.datum(index as u32, &format!("token.{i}"), datum, 0)?
                });
            }
            document.library.0.extend(values.library);
            Expr::Tuple(parts)
        }
        _ => return Err(MaterializeRefusal::Callable(FunctionValueRefusal::Identity)),
    };
    body.push(binding(raw_name.clone(), raw));
    let parts = vec![Expr::Variable(raw_name), Expr::Variable(setter_name)];
    body.push(Stmt::Return {
        value: if ts {
            Expr::List(parts)
        } else {
            Expr::Tuple(parts)
        },
    });
    source::closure(
        document,
        format!("saved_factory_{index}"),
        vec![],
        body,
        abi,
        helpers,
        fresh,
    )
}
