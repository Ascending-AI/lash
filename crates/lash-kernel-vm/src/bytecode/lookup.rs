//! Compose a guarded map read and the following word helper without moving
//! their ops or merging their bills. Both proofs come from `fold`.

use lash_kernel_doc::{Measure, Value};
use lash_kernel_value::Type;

use super::charge_tables::{Charge, RESULT};

use super::op::Op;
use super::program::{Callee, Constant, LookupWords, Program, Slot, Step, Unit};

pub(super) fn derive(program: &mut Program) {
    for at in 0..program.units.len() {
        let found: Vec<_> = (0..program.units[at].inlines.len())
            .filter_map(|n| recipe(program, &program.units[at], n).map(|recipe| (n, recipe)))
            .collect();
        let unit = &mut program.units[at];
        for (n, recipe) in found {
            unit.inlines[n].lookup_words = Some(unit.lookup_words.len() as u32);
            unit.lookup_words.push(recipe);
        }
    }
}

fn recipe(program: &Program, unit: &Unit, n: usize) -> Option<LookupWords> {
    let site = unit.inlines[n];
    let call = &unit.calls[site.call as usize];
    let Callee::Unit {
        unit: get_callee,
        charge: Some(get_charge),
    } = call.callee
    else {
        return None;
    };
    if call.argc != 2 {
        return None;
    }
    // With depth above one, the runtime map/text guard is exactly the
    // existing shallow argument check for these declared types.
    if let Some(checked) = &call.checked {
        let params = &checked.signature.params;
        if checked.required > 2
            || params.len() < 2
            || !matches!(
                checked.signature.result,
                Type::Any | Type::Int | Type::Number
            )
        {
            return None;
        }
        let map = matches!(&params[0].ty, Type::Any)
            || matches!(&params[0].ty, Type::Map(map) if map.key == Type::Any && map.value == Type::Any);
        if !map || !matches!(&params[1].ty, Type::Any | Type::Text) {
            return None;
        }
    }
    let [lookup] = &unit.folds[site.folds.0 as usize..site.folds.1 as usize] else {
        return None;
    };
    if lookup.len != 1
        || lookup.result != Slot::Made(0)
        || lookup.steps[0]
            != (Step::Lookup {
                target: Slot::Local(0),
                key: Slot::Local(1),
            })
    {
        return None;
    }
    let pc = site.at as usize;
    let Op::Const(k) = *unit.ops.get(pc.checked_sub(1)?)? else {
        return None;
    };
    let Constant::Immediate(Value::Text(key)) = program.get_constant(k)? else {
        return None;
    };
    let [
        Op::Block(between),
        Op::Inline(next),
        Op::Block(after),
        Op::SetLocal(destination),
    ] = unit.ops.get(pc + 1..pc + 5)?
    else {
        return None;
    };
    let between = unit.blocks[*between as usize];
    let after = unit.blocks[*after as usize];
    if between.start as usize != pc + 1
        || between.end as usize != pc + 3
        || after.start as usize != pc + 3
        || unit.step[pc + 1..pc + 5].iter().any(|step| *step)
        || unit
            .points
            .iter()
            .any(|(at, _)| (pc + 1..pc + 5).contains(&(*at as usize)))
    {
        return None;
    }
    let next = unit.inlines[*next as usize];
    let following = &unit.calls[next.call as usize];
    let Callee::Unit {
        unit: word_callee,
        charge: Some(word_charge),
    } = following.callee
    else {
        return None;
    };
    if following.argc != 2 || following.entry.is_some() || following.checked.is_some() {
        return None;
    }
    let words = unit.folds.get(next.folds.0 as usize)?;
    let form = words.words?;
    if next.folds.0 == next.folds.1 || form.operands != [Slot::Local(0), Slot::Local(1)] {
        return None;
    }
    Some(LookupWords {
        key: *key,
        lookup: site.folds.0,
        words: next.folds.0,
        get_callee,
        word_callee,
        get_fees: lookup_fees(program.charge(get_charge), Value::Text(*key))?,
        word_fees: program.charge(word_charge).pairs()?,
        operation: form.operation,
        between: between.charge?,
        after: after.charge?,
        resume: pc as u32 + 5,
        destination: *destination,
    })
}

/// The getter's formula over its constant key and a word result, only
/// where it reads no receiver. Each measurement comes from that value.
fn lookup_fees(charge: &Charge, key: Value) -> Option<[u64; 2]> {
    let mut fees = [0; 2];
    for (nonzero, fee) in fees.iter_mut().enumerate() {
        let result = Value::Int(nonzero as i64);
        let mut known = true;
        *fee = charge.evaluate(|position, how| {
            if position == RESULT && how == Measure::Magnitude {
                known = false;
                return 0;
            }
            let value = match position {
                1 => &key,
                RESULT => &result,
                _ => {
                    known = false;
                    return 0;
                }
            };
            crate::data::immediate_measure(value, how).unwrap_or_else(|| {
                known = false;
                0
            })
        });
        if !known {
            return None;
        }
    }
    Some(fees)
}
