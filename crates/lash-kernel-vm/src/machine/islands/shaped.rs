//! Operand-shaped island nodes. Compile-time shape selection leaves the
//! runtime read, commit and call protocol in the same evaluation order.

use super::*;

/// A call's typed `(Slot, Const)` argument, optionally preceded by a
/// constant receiver. Keeping its site and operands together lets the call
/// evaluate it without another node entry.
pub(super) struct CallArgument {
    site: OperatorSite,
    pair: lash_kernel_doc::scalar::Pair,
    at: usize,
    miss: u32,
    constant: Value,
    covered: bool,
    /// A constant leading argument, such as TypeScript's absent receiver.
    prefix: Option<Value>,
}

pub(super) fn call_argument(args: &[Expr], cx: &mut Build<'_>) -> Option<CallArgument> {
    let (prefix, argument) = match args {
        [argument] => (None, argument),
        [
            Expr {
                kind: Kind::Const(prefix),
                ..
            },
            argument,
        ] => (Some(*prefix), argument),
        _ => return None,
    };
    let Kind::Operator { args, .. } = &argument.kind else {
        return None;
    };
    let [
        Expr {
            kind: Kind::Local { local, miss, .. },
            ..
        },
        Expr {
            kind: Kind::Const(constant),
            ..
        },
    ] = &args[..]
    else {
        return None;
    };
    let site = operator_site(argument, cx.exe)?;
    let pair = site.entry.pair()?;
    cx.operator(&site);
    let covered = cx.cover(site.fixed.saturating_add(site.table.max()));
    if !covered {
        cx.end();
    }
    Some(CallArgument {
        site,
        pair,
        at: local.at as usize,
        miss: *miss,
        constant: *constant,
        covered,
        prefix,
    })
}

pub(super) fn fused_call(site: CallSite, read: Option<Operand>, argument: CallArgument) -> Node {
    if argument.covered {
        if argument.site.counted.is_some() {
            fused_call_node::<true, true>(site, read, argument)
        } else {
            fused_call_node::<true, false>(site, read, argument)
        }
    } else {
        fused_call_node::<false, false>(site, read, argument)
    }
}

fn fused_call_node<const COVERED: bool, const COUNTED: bool>(
    site: CallSite,
    read: Option<Operand>,
    argument: CallArgument,
) -> Node {
    match argument.prefix {
        Some(prefix) => {
            fused_node::<COVERED, COUNTED, 2>(site, read, argument, [prefix, Value::Absent])
        }
        None => fused_node::<COVERED, COUNTED, 1>(site, read, argument, [Value::Absent]),
    }
}

fn fused_node<const COVERED: bool, const COUNTED: bool, const ARITY: usize>(
    site: CallSite,
    read: Option<Operand>,
    argument: CallArgument,
    prefix: [Value; ARITY],
) -> Node {
    Box::new(move |island, slots| {
        let callee = match &read {
            Some(read) => Some(read.eval(island, slots)?),
            None => None,
        };
        let value = match slots.get(argument.at) {
            Some(Some(value)) => typed_commit::<COVERED, COUNTED>(
                &argument.site,
                &argument.pair,
                island,
                *value,
                argument.constant,
            ),
            _ => Err(island.at(argument.miss)),
        };
        let x = match value {
            Ok(x) => x,
            Err(stop) => {
                return Err(island.call_held(stop, &site, callee, &prefix[..ARITY - 1]));
            }
        };
        let mut values = prefix;
        values[ARITY - 1] = x;
        island.call(&site, callee, &values)
    })
}

/// The compiled typed operator `site` over `a` and `b` in a segment, which
/// counts an occurrence where `COUNTED`: in an admitted segment its typed
/// path commits unchecked, as [`Island::typed`] commits it, its close, its
/// occurrence, then its formula; where it leaves the path, the segment is
/// checked from there.
pub(super) fn typed_operator<const COVERED: bool, const COUNTED: bool>(
    site: OperatorSite,
    pair: lash_kernel_doc::scalar::Pair,
    a: Operand,
    b: Operand,
) -> Node {
    match (a, b) {
        (Operand::Slot { at, miss }, Operand::Const(y)) => Box::new(move |island, slots| {
            let x = match slots.get(at) {
                Some(Some(value)) => *value,
                _ => return Err(island.at(miss)),
            };
            typed_commit::<COVERED, COUNTED>(&site, &pair, island, x, y)
        }),
        (Operand::Slot { at: p, miss: pm }, Operand::Slot { at: q, miss: qm }) => {
            Box::new(move |island, slots| {
                let x = match slots.get(p) {
                    Some(Some(value)) => *value,
                    _ => return Err(island.at(pm)),
                };
                let y = match slots.get(q) {
                    Some(Some(value)) => *value,
                    _ => {
                        let stop = island.at(qm);
                        return Err(island.held(stop, site.reg, x));
                    }
                };
                typed_commit::<COVERED, COUNTED>(&site, &pair, island, x, y)
            })
        }
        (a, b) => Box::new(move |island, slots| {
            let x = a.eval(island, slots)?;
            let y = match b.eval(island, slots) {
                Ok(y) => y,
                Err(stop) => return Err(island.held(stop, site.reg, x)),
            };
            typed_commit::<COVERED, COUNTED>(&site, &pair, island, x, y)
        }),
    }
}

#[inline(always)]
fn typed_commit<const COVERED: bool, const COUNTED: bool>(
    site: &OperatorSite,
    pair: &lash_kernel_doc::scalar::Pair,
    island: &mut Island<'_>,
    x: Value,
    y: Value,
) -> Answer {
    if let Some((value, units)) = typed(site, pair, x, y) {
        if COVERED
            && island.covered
            && let Some(units) = units
        {
            island.charged += site.fixed;
            if COUNTED && let Some(occurrence) = site.counted {
                island.count(occurrence)?;
            }
            island.charged += units;
            return Ok(value);
        }
        if let Some(answer) = island.typed(site, value, units) {
            return answer;
        }
    }
    if COVERED {
        island.covered = false;
    }
    island.operate(site, &[x, y])
}
