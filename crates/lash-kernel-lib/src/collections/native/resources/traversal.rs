//! Input-only graph projection. No native execution or nursery is used here.
use super::*;
use crate::collections::native::{items, raise};
use lash_kernel_doc::{NativeResourceRelease as Release, arena::ArenaHandle};
use std::collections::HashMap;

struct Plan<'a> {
    args: Args<'a>,
    events: Vec<Event>,
    scratch: u64,
    active: Vec<ArenaHandle>,
    copied: HashMap<(u32, u32), Holding>,
}
impl<'a> Plan<'a> {
    fn new(args: Args<'a>) -> Self {
        Self {
            args,
            events: vec![],
            scratch: 0,
            active: vec![],
            copied: HashMap::new(),
        }
    }
    fn visit(&mut self, value: &Value) -> Result<(), NativeError> {
        // One traversal occurrence and its continuation. This is cumulative,
        // hence bounds both the active frontier and completed projection data.
        self.scratch = self
            .scratch
            .checked_add(Cost::native_local(self.args, value)?)
            .and_then(|n| n.checked_add(4 * Cost::REFERENCE))
            .ok_or_else(overflow)?;
        Ok(())
    }
    fn entity(&mut self, kind: Entity, members: Vec<Holding>) -> Result<Holding, NativeError> {
        let owner = Owner(u32::try_from(self.events.len()).map_err(|_| overflow())?);
        self.events.push(Event::Entity {
            owner,
            kind,
            members,
            release: None,
        });
        Ok(Holding {
            bytes: Cost::REFERENCE,
            origin: Origin::Fresh(owner),
        })
    }
    fn finish(mut self, answer: Result<Holding, NativeError>) -> Result<Contract, NativeError> {
        let (exit, output) = match answer {
            Ok(output) => (Exit::Returned, output),
            Err(NativeError::Refused { kind, message }) => {
                for event in &mut self.events {
                    if let Event::Entity { release, .. } = event {
                        *release = Some(Release::Exit);
                    }
                }
                (
                    Exit::Raised,
                    Contract::refused(kind, &message)
                        .ok_or_else(overflow)?
                        .output,
                )
            }
            Err(error) => return Err(error),
        };
        let mut contract = Contract {
            exit,
            output,
            events: self.events,
        };
        crate::resource_contracts::scratch(&mut contract, self.scratch)?;
        Ok(contract)
    }

    fn copy(&mut self, value: &Value, depth: usize) -> Result<Holding, NativeError> {
        let limit = self.args.limits().value_depth().min(128);
        if depth > limit {
            return Err(raise(
                "too_deep",
                format!("deep copy exceeds the configured {limit} levels"),
            ));
        }
        self.visit(value)?;
        match value {
            Value::Tuple(_) => {
                let mut members = vec![];
                let values = items(&self.args, value)?;
                for value in &values {
                    self.visit(value)?;
                }
                for value in values {
                    members.push(self.copy(&value, depth + 1)?);
                }
                tuple(&members)
            }
            Value::Error(_) => {
                let error = self.args.error(value)?.ok_or_else(overflow)?;
                let data = self.copy(&error.data, depth + 1)?;
                let bytes = self
                    .args
                    .text(&error.kind)?
                    .ok_or_else(overflow)?
                    .len()
                    .checked_add(self.args.text(&error.message)?.ok_or_else(overflow)?.len())
                    .and_then(|n| n.checked_add(Cost::REFERENCE))
                    .and_then(|n| n.checked_add(data.bytes))
                    .ok_or_else(overflow)?;
                Ok(Holding {
                    bytes,
                    origin: Origin::Immutable,
                })
            }
            Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
                if self.active.contains(id) {
                    return Err(raise(
                        "cyclic_value",
                        "deep copy refuses a collection cycle",
                    ));
                }
                if let Some(holding) = self.copied.get(&(id.index(), id.generation())) {
                    return Ok(*holding);
                }
                self.active.push(*id);
                let mut members = vec![];
                let kind = if matches!(value, Value::List(_)) {
                    let values = items(&self.args, value)?;
                    for value in &values {
                        self.visit(value)?;
                    }
                    for value in values {
                        members.push(self.copy(&value, depth + 1)?);
                    }
                    Entity::List
                } else {
                    let mut names = 0_u64;
                    let pairs = entries(&self.args, value)?;
                    for (key, item) in &pairs {
                        self.visit(key)?;
                        self.visit(item)?;
                    }
                    for (key, item) in pairs {
                        if matches!(value, Value::Record(_)) {
                            names = names
                                .checked_add(self.args.text(&key)?.ok_or_else(overflow)?.len())
                                .ok_or_else(overflow)?;
                        } else {
                            members.push(self.copy(&key, depth + 1)?);
                        }
                        members.push(if matches!(value, Value::Set(_)) {
                            holding(self.args, &Value::Absent)?
                        } else {
                            self.copy(&item, depth + 1)?
                        });
                    }
                    match value {
                        Value::Set(_) => Entity::Set,
                        Value::Map(_) => Entity::Map,
                        _ => Entity::Record {
                            field_name_bytes: names,
                        },
                    }
                };
                let result = self.entity(kind, members)?;
                self.active.pop();
                self.copied.insert((id.index(), id.generation()), result);
                Ok(result)
            }
            _ => holding(self.args, value),
        }
    }
    fn identity(
        &mut self,
        value: &Value,
        absent: &Value,
        tag: &Value,
        depth: usize,
    ) -> Result<Holding, NativeError> {
        if depth > self.args.limits().value_depth().min(128) {
            return Err(raise(
                "too_deep",
                "identity key exceeds the configured value depth",
            ));
        }
        self.visit(value)?;
        if let Some(projected) = super::super::scalar_identity(value, absent) {
            return holding(self.args, &projected);
        }
        if matches!(value, Value::Tuple(_)) {
            let mut projected = vec![];
            let values = items(&self.args, value)?;
            for value in &values {
                self.visit(value)?;
            }
            for item in values {
                let child = self.identity(&item, absent, tag, depth + 1)?;
                self.scratch = self.scratch.checked_add(child.bytes).ok_or_else(overflow)?;
                projected.push(child);
            }
            return tuple(&projected);
        }
        let error = self.args.error(value)?.ok_or_else(overflow)?;
        let data = super::super::reference(&error.data)
            .ok_or_else(|| raise("type_error", "ref requires an object or task"))?;
        tuple(&[
            holding(self.args, tag)?,
            holding(self.args, &data)?,
            holding(self.args, &error.kind)?,
            holding(self.args, &error.message)?,
        ])
    }
}
pub(super) fn deep(args: Args<'_>, value: &Value) -> Result<Contract, NativeError> {
    let mut plan = Plan::new(args);
    let answer = plan.copy(value, 0);
    plan.finish(answer)
}
pub(super) fn identity(args: Args<'_>) -> Result<Contract, NativeError> {
    let mut plan = Plan::new(args);
    let answer = (|| {
        let value = *nth(args.values(), 0)?;
        let absent = *nth(args.values(), 1)?;
        let tag = *field(nth(args.values(), 2)?)?;
        plan.identity(&value, &absent, &tag, 0)
    })();
    plan.finish(answer)
}

pub(super) fn holds_none(args: Args<'_>) -> Result<Contract, NativeError> {
    let mut plan = Plan::new(args);
    let answer = (|| {
        let kinds = nth(args.values(), 1)?;
        let (Value::Set(_), Some(table)) = (kinds, args.table(kinds)?) else {
            return Err(raise("type_error", "kinds must be a set of kind names"));
        };
        let mut refused = [false; crate::numbers::KIND_NAMES.len()];
        for chunk in table.chunks() {
            for pair in chunk?.as_chunks::<2>().0 {
                let named = match args.text(&pair[0])? {
                    Some(name) => {
                        let name = name.whole()?;
                        crate::numbers::KIND_NAMES
                            .iter()
                            .position(|(_, kind)| *kind == &*name)
                    }
                    None => None,
                };
                match named {
                    Some(index) => refused[index] = true,
                    None => return Err(raise("type_error", "kinds must be a set of kind names")),
                }
            }
        }
        fn walk(
            plan: &mut Plan<'_>,
            value: &Value,
            depth: usize,
            refused: &[bool],
            seen: &mut Vec<ArenaHandle>,
        ) -> Result<bool, NativeError> {
            plan.visit(value)?;
            if depth > plan.args.limits().value_depth().min(128) || refused[value.kind() as usize] {
                return Ok(false);
            }
            let mut members = vec![];
            match value {
                Value::Tuple(_) => members = items(&plan.args, value)?,
                Value::Error(_) => members.push(plan.args.error(value)?.ok_or_else(overflow)?.data),
                Value::List(id) | Value::Set(id) | Value::Map(id) | Value::Record(id) => {
                    if plan.active.contains(id) {
                        return Ok(false);
                    }
                    if seen.contains(id) {
                        return Ok(true);
                    }
                    plan.active.push(*id);
                    if matches!(value, Value::List(_)) {
                        members = items(&plan.args, value)?;
                    } else {
                        for (key, item) in entries(&plan.args, value)? {
                            if !matches!(value, Value::Record(_)) {
                                members.push(key);
                            }
                            if !matches!(value, Value::Set(_)) {
                                members.push(item);
                            }
                        }
                    }
                }
                _ => {}
            }
            for member in &members {
                plan.visit(member)?;
            }
            for member in members {
                if !walk(plan, &member, depth + 1, refused, seen)? {
                    return Ok(false);
                }
            }
            if matches!(
                value,
                Value::List(_) | Value::Set(_) | Value::Map(_) | Value::Record(_)
            ) {
                plan.active.pop();
                seen.push(value.handle().ok_or_else(overflow)?);
            }
            Ok(true)
        }
        let value = *nth(args.values(), 0)?;
        let result = walk(&mut plan, &value, 0, &refused, &mut vec![])?;
        holding(args, &Value::Bool(result))
    })();
    plan.finish(answer)
}
