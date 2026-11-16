//! Resident accesses selected from the exact helper body and its installed
//! plan. No function name selects an operation.

use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::arena::native::InlineOperation;
use lash_kernel_doc::plan::{Element, Table};
use lash_kernel_doc::{Expr, FunctionRegistry, Literal, Member, Place, Rhs, Stmt};
use lash_kernel_value::names::{FunctionId, Name};

use super::program::Access;

pub(super) fn derive(registry: &FunctionRegistry, id: FunctionId) -> Option<Access> {
    let registered = registry.get(&id)?;
    let plan = registered.plan()?;
    let heap = plan.template.as_ref()?.heap.as_ref()?;
    if registered.native().is_some() || registered.definition().guard.is_some() {
        return None;
    }
    if let Some(path) = heap.element.as_ref()
        && path.requires.iter().all(|requirement| {
            registry.get(&requirement.function).is_some_and(|callee| {
                callee.definition().guard.is_none()
                    && callee
                        .native()
                        .is_some_and(|native| requirement.fact.accepts(&native.specialization()))
            })
        })
    {
        match path.operation {
            Element::Read => return Some(Access::ListRead { depth: path.depth }),
            Element::ReadMap if python_map_read(registry, registered.definition()).is_some() => {
                return Some(Access::MapRead {
                    depth: path.depth,
                    room: path.room,
                });
            }
            _ => {}
        }
    }
    heap.record_write?;
    let definition = registered.definition();
    let [target, key, value] = definition.signature.params.as_slice() else {
        return None;
    };
    let [
        Stmt::Let {
            name: kind,
            value: Rhs::Expr(kind_call),
        },
        Stmt::If {
            condition,
            then_block,
            ..
        },
        ..,
    ] = definition.body()?.block.as_slice()
    else {
        return None;
    };
    let [operand] = call(registry, kind_call, InlineOperation::Kind)? else {
        return None;
    };
    variable(operand, &target.name)?;
    let [left, Expr::Literal(Literal::Text(record))] =
        call(registry, condition, InlineOperation::Same)?
    else {
        return None;
    };
    variable(left, kind)?;
    if record != "record" {
        return None;
    }
    let [Stmt::Try(scope)] = then_block.as_slice() else {
        return None;
    };
    if scope.finally.is_some() {
        return None;
    }
    let [
        Stmt::If {
            condition,
            then_block,
            ..
        },
    ] = scope.body.as_slice()
    else {
        return None;
    };
    let [contains] = call(registry, condition, InlineOperation::Not)? else {
        return None;
    };
    let [operand, Expr::Literal(Literal::Text(marker))] = call(
        registry,
        contains,
        InlineOperation::Membership(Table::Record),
    )?
    else {
        return None;
    };
    variable(operand, &target.name)?;
    let [
        Stmt::Assign {
            place: Place::Member(member),
            value: Rhs::Expr(assigned),
            ..
        },
        Stmt::Return { value: returned },
    ] = then_block.as_slice()
    else {
        return None;
    };
    let Member::Index {
        target: written,
        index,
    } = member
    else {
        return None;
    };
    variable(written, &target.name)?;
    variable(index, &key.name)?;
    variable(assigned, &value.name)?;
    variable(returned, &value.name)?;
    Some(Access::RecordWrite {
        key: ShortText::new(marker)?,
        // The body and its native guard calls. Near the bound the source
        // makes the exact check and reports the original callee.
        depth: 2,
    })
}

fn variable(expr: &Expr, name: &Name) -> Option<()> {
    matches!(expr, Expr::Variable(found) if found == name).then_some(())
}

fn call<'a>(
    registry: &FunctionRegistry,
    expr: &'a Expr,
    operation: InlineOperation,
) -> Option<&'a [Expr]> {
    let Expr::Call { function, args } = expr else {
        return None;
    };
    let callee = registry.get(function)?;
    if callee.definition().guard.is_some() || callee.has_body() {
        return None;
    }
    (callee
        .native()?
        .specialization()
        .inline?
        .for_version(callee.definition().native_version)
        == Some(operation))
    .then_some(args)
}

/// The early map-hit arm, over unchanged arguments. Each primitive's
/// installed native must still supply the operation the source proof used.
fn python_map_read(
    registry: &FunctionRegistry,
    definition: &lash_kernel_doc::FunctionDefinition,
) -> Option<()> {
    let [target, index] = definition.signature.params.as_slice() else {
        return None;
    };
    let [
        Stmt::Let {
            name: kind,
            value: Rhs::Expr(kind_call),
        },
        Stmt::If {
            condition,
            then_block,
            ..
        },
        ..,
    ] = definition.body()?.block.as_slice()
    else {
        return None;
    };
    let [operand] = call(registry, kind_call, InlineOperation::Kind)? else {
        return None;
    };
    variable(operand, &target.name)?;
    let [left, Expr::Literal(Literal::Text(map))] =
        call(registry, condition, InlineOperation::Same)?
    else {
        return None;
    };
    variable(left, kind)?;
    if map != "map" {
        return None;
    }
    let [
        Stmt::If {
            condition,
            else_block,
            ..
        },
    ] = then_block.as_slice()
    else {
        return None;
    };
    let [kind_call, Expr::Literal(Literal::Text(tuple))] =
        call(registry, condition, InlineOperation::Same)?
    else {
        return None;
    };
    if tuple != "tuple" {
        return None;
    }
    let [operand] = call(registry, kind_call, InlineOperation::Kind)? else {
        return None;
    };
    variable(operand, &index.name)?;
    let [
        Stmt::If {
            condition,
            then_block,
            ..
        },
    ] = else_block.as_slice()
    else {
        return None;
    };
    let [receiver, key] = call(registry, condition, InlineOperation::Membership(Table::Map))?
    else {
        return None;
    };
    variable(receiver, &target.name)?;
    variable(key, &index.name)?;
    let [
        Stmt::Return {
            value: Expr::Member(member),
        },
    ] = then_block.as_slice()
    else {
        return None;
    };
    let Member::Index {
        target: receiver,
        index: key,
    } = member.as_ref()
    else {
        return None;
    };
    variable(receiver, &target.name)?;
    variable(key, &index.name)
}
