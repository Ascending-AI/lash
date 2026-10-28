//! Input-only regex allocation schedule. Matching facts are read without a nursery.
//! Compiled programs and compiler work are code-owned; each live program reference
//! is 16 bytes. UTF-16 subjects, match tables, capture arrays and replacement
//! buffers are run-owned logical scratch, independent of Rust capacities.
use super::*;
use lash_kernel_doc::{
    LogicalHeldV1 as Cost, NativeResourceContract as Contract, NativeResourceEntity as Entity,
    NativeResourceEvent as Event, NativeResourceExit as Exit, NativeResourceHolding as Holding,
    NativeResourceOrigin as Origin, NativeResourceOwner as Owner, NativeResourceRelease as Release,
};

fn immutable(payload: u64) -> Holding {
    Holding {
        bytes: Cost::REFERENCE + payload,
        origin: Origin::Immutable,
    }
}
fn integer(value: &Integer) -> Holding {
    immutable(value.bits().div_ceil(8))
}
fn entity(plan: &mut Contract, kind: Entity, members: Vec<Holding>) -> Holding {
    let owner = Owner(plan.events.len() as u32);
    plan.events.push(Event::Entity {
        owner,
        kind,
        members,
        release: None,
    });
    Holding {
        bytes: Cost::REFERENCE,
        origin: Origin::Fresh(owner),
    }
}
fn record(plan: &mut Contract, fields: Vec<(&str, Holding)>) -> Holding {
    entity(
        plan,
        Entity::Record {
            field_name_bytes: fields.iter().map(|(name, _)| name.len() as u64).sum(),
        },
        fields.into_iter().map(|(_, value)| value).collect(),
    )
}
fn scratch(plan: &mut Contract, bytes: u64) {
    plan.events.push(Event::Scratch {
        owner: Owner(plan.events.len() as u32),
        bytes,
        release: Release::Exit,
    });
}
fn text(args: Args<'_>, value: &Value, label: &str) -> Result<String, NativeError> {
    args.text(value)?
        .ok_or_else(|| wrong_type(label, "a text", value))?
        .whole()
        .map(|text| text.into_owned())
}
fn regex(args: Args<'_>, value: &Value) -> Result<Regex, NativeError> {
    let (Value::Record(_), Some(fields)) = (value, args.table(value)?) else {
        return Err(wrong_type("regex", "a regex record", value));
    };
    let field = |key| args.lookup(fields, &name(key));
    let (
        Some(brand @ Value::Tuple(_)),
        Some(pattern @ (Value::Text(_) | Value::LongText(_))),
        Some(flags @ (Value::Text(_) | Value::LongText(_))),
        Some(last_index @ (Value::Int(_) | Value::BigInt(_))),
    ) = (
        field("__lash_brand")?,
        field("pattern")?,
        field("flags")?,
        field("lastIndex")?,
    )
    else {
        return Err(not_a_regex());
    };
    let brand = args.sequence(brand)?.ok_or_else(not_a_regex)?;
    if brand.len() != 2
        || fields.len() != 5
        || !args.equal(brand.get(0)?.ok_or_else(not_a_regex)?, &name(BRAND))?
        || !args.equal(brand.get(1)?.ok_or_else(not_a_regex)?, &name("reg_exp"))?
        || !matches!(field("__lash_built")?, Some(Value::Bytes(_)))
    {
        return Err(not_a_regex());
    }
    let last_index = args.integer(last_index)?.ok_or_else(not_a_regex)?;
    if last_index.is_negative() {
        return Err(raised(
            "number_range",
            "the regex's `lastIndex` is negative: it counts UTF-16 code units from the start of the input",
        ));
    }
    Ok(Regex {
        pattern: text(args, pattern, "pattern")?,
        flags: Flags::parse(&text(args, flags, "flags")?).map_err(syntax_error)?,
        last_index: last_index.into_owned(),
    })
}

pub(super) fn contract(native: &Function, args: Args<'_>) -> Result<Option<Contract>, NativeError> {
    let mut plan = Contract::value(Exit::Returned, Cost::REFERENCE, Origin::Immutable);
    match evaluate(native, args, &mut plan) {
        Ok(output) => plan.output = output,
        Err(NativeError::Refused { kind, message }) => {
            plan.exit = Exit::Raised;
            plan.output = Contract::refused(kind, &message)
                .ok_or(NativeError::Malformed("native logical cost overflow"))?
                .output;
            // A late surrogate error retires every partially built match entity.
            for event in &mut plan.events {
                if let Event::Entity { release, .. } = event {
                    *release = Some(Release::Exit);
                }
            }
        }
        Err(NativeError::Guard(_)) => return Ok(None),
        Err(error) => return Err(error),
    }
    Ok(Some(plan))
}
fn evaluate(
    native: &Function,
    args: Args<'_>,
    plan: &mut Contract,
) -> Result<Holding, NativeError> {
    let arg = |index| args.get(index).unwrap_or(&Value::Absent);
    if native.operation == Operation::CompileCheck {
        let pattern = text(args, arg(0), "pattern")?;
        let flags = text(args, arg(1), "flags")?;
        // Compiler storage belongs to admitted code, including cached failures.
        scratch(plan, Cost::REFERENCE);
        let flags = native
            .engine
            .check(&pattern, &flags)
            .map_err(syntax_error)?;
        scratch(plan, Cost::REFERENCE + flags.canonical().len() as u64);
        if let Event::Scratch { release, .. } = &mut plan.events[0] {
            *release = Release::Before(Owner(1));
        }
        return Ok(record(
            plan,
            vec![
                (
                    "__lash_brand",
                    immutable(2 * Cost::REFERENCE + BRAND.len() as u64 + 7),
                ),
                ("__lash_built", immutable(32)),
                ("pattern", immutable(pattern.len() as u64)),
                ("flags", immutable(flags.canonical().len() as u64)),
                ("lastIndex", integer(&Integer::new(0))),
            ],
        ));
    }
    let regex = regex(args, arg(0))?;
    let input = text(args, arg(1), "input")?;
    scratch(plan, Cost::REFERENCE);
    let program = native
        .engine
        .program(&regex.pattern, regex.flags)
        .map_err(syntax_error)?;
    let units: Vec<_> = input.encode_utf16().collect();
    scratch(plan, Cost::REFERENCE + 2 * units.len() as u64);
    let names = if regex.flags.has_indices {
        capture_names(&regex.pattern)
    } else {
        Vec::new()
    };
    scratch(
        plan,
        Cost::OBJECT
            + names
                .iter()
                .map(|(name, index)| {
                    2 * Cost::REFERENCE
                        + name.len() as u64
                        + Integer::new(*index).bits().div_ceil(8)
                })
                .sum::<u64>(),
    );
    let subject = Subject {
        search: Search {
            program: &program,
            units: &units,
            unicode: regex.flags.unicode,
        },
        capture_names: names,
        regex: &regex,
    };
    let guard = crate::GUARD_BASE
        .saturating_add(crate::GUARD_PER_INPUT_UNIT.saturating_mul(1 + input.len() as u64));
    let mut counter = WorkCounter::new(Some(guard));
    match native.operation {
        Operation::CompileCheck => unreachable!("answered above"),
        Operation::Exec | Operation::Test => {
            let (found, last_index) = subject.exec(&mut counter)?;
            scratch(plan, found.as_ref().map_or(0, found_bytes));
            let result = if native.operation == Operation::Exec {
                match found {
                    Some(found) => {
                        spend_text(&mut counter, match_units(&found))?;
                        match_value(plan, &subject, &found)?
                    }
                    None => immutable(0),
                }
            } else {
                immutable(0)
            };
            Ok(record(
                plan,
                vec![
                    (
                        if native.operation == Operation::Exec {
                            "match"
                        } else {
                            "matched"
                        },
                        result,
                    ),
                    ("lastIndex", integer(&last_index)),
                ],
            ))
        }
        Operation::MatchAll | Operation::Replace => {
            let replacement = if native.operation == Operation::Replace {
                Some(text(args, arg(2), "replacement")?)
            } else {
                None
            };
            let mut matches = Vec::new();
            let last_index = if regex.flags.global {
                let start = if replacement.is_some() {
                    Some(0)
                } else {
                    regex
                        .last_index
                        .to_usize()
                        .filter(|start| *start <= units.len())
                };
                if let Some(start) = start {
                    subject
                        .search
                        .each(start, regex.flags.sticky, &mut counter, |found| {
                            matches.push(found);
                            ControlFlow::Continue(())
                        })?;
                }
                Integer::new(0)
            } else {
                let (found, index) = subject.exec(&mut counter)?;
                matches.extend(found);
                index
            };
            scratch(
                plan,
                Cost::OBJECT + matches.iter().map(found_bytes).sum::<u64>(),
            );
            if let Some(replacement) = replacement {
                // A named substitution temporarily owns the identifier parsed from the template.
                scratch(plan, Cost::REFERENCE + replacement.len() as u64);
                let output = replacement_units(&replacement, &units, &matches, &mut counter)?;
                scratch(
                    plan,
                    Cost::REFERENCE
                        + 2 * output.len() as u64
                        + Cost::REFERENCE
                        + 4 * replacement.chars().count() as u64,
                );
                let text = text_holding(plan, &output)?;
                Ok(record(
                    plan,
                    vec![("text", text), ("lastIndex", integer(&last_index))],
                ))
            } else {
                spend_text(&mut counter, matches.iter().map(match_units).sum())?;
                let mut values = Vec::new();
                for found in matches {
                    values.push(match_value(plan, &subject, &found)?);
                }
                Ok(entity(plan, Entity::List, values))
            }
        }
        Operation::Split => {
            let limit = match arg(2) {
                Value::Absent => usize::MAX,
                value @ (Value::Int(_) | Value::BigInt(_)) => {
                    let limit = args
                        .integer(value)?
                        .ok_or_else(|| wrong_type("limit", "an integer", value))?;
                    if limit.is_negative() {
                        return Err(raised(
                            "number_range",
                            "`limit` is negative: give the most pieces to return",
                        ));
                    }
                    limit.to_usize().unwrap_or(usize::MAX)
                }
                other => return Err(wrong_type("limit", "an integer", other)),
            };
            let mut pieces = Vec::new();
            let mut found_peak = 0;
            if limit != 0 {
                if units.is_empty() {
                    let found = subject.search.first(0, false, &mut counter)?;
                    found_peak = found.as_ref().map_or(0, found_bytes);
                    if found.is_none() {
                        pieces.push(Some(0..0));
                    }
                } else {
                    let mut end = 0;
                    subject.search.each(0, false, &mut counter, |found| {
                        found_peak = found_peak.max(found_bytes(&found));
                        if found.range.start == units.len() {
                            return ControlFlow::Break(());
                        }
                        if found.range.is_empty() && found.range.start == end {
                            return ControlFlow::Continue(());
                        }
                        pieces.push(Some(end..found.range.start));
                        pieces.extend(found.captures);
                        end = found.range.end;
                        if pieces.len() >= limit {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    })?;
                    if pieces.len() < limit {
                        pieces.push(Some(end..units.len()));
                    }
                }
            }
            // The pre-truncation table and one current match may overlap.
            scratch(
                plan,
                Cost::OBJECT + pieces.iter().map(range_bytes).sum::<u64>() + found_peak,
            );
            pieces.truncate(limit);
            spend_text(
                &mut counter,
                pieces
                    .iter()
                    .flatten()
                    .map(|range| range.len() as u64)
                    .sum(),
            )?;
            let mut values = Vec::new();
            for piece in pieces {
                values.push(match piece {
                    None => immutable(0),
                    Some(range) => text_holding(plan, &units[range])?,
                });
            }
            Ok(entity(plan, Entity::List, values))
        }
    }
}
fn range_bytes(range: &Option<Range<usize>>) -> u64 {
    range.as_ref().map_or(Cost::REFERENCE, |range| {
        Cost::REFERENCE
            + 2 * Cost::REFERENCE
            + Integer::new(range.start).bits().div_ceil(8)
            + Integer::new(range.end).bits().div_ceil(8)
    })
}
fn found_bytes(found: &Found) -> u64 {
    Cost::OBJECT
        + range_bytes(&Some(found.range.clone()))
        + Cost::OBJECT
        + found.captures.iter().map(range_bytes).sum::<u64>()
        + Cost::OBJECT
        + found
            .named
            .iter()
            .map(|(name, range)| name.len() as u64 + range_bytes(range))
            .sum::<u64>()
}
fn text_holding(plan: &mut Contract, units: &[u16]) -> Result<Holding, NativeError> {
    let text = text_from_units(units).ok_or_else(lone_surrogate)?;
    // A decoded text buffer lasts only until its immutable occurrence is built.
    let owner = Owner(plan.events.len() as u32);
    scratch(plan, Cost::REFERENCE + text.len() as u64);
    let next = Owner(owner.0 + 1);
    // An explicit zero-byte boundary lets consecutive decode buffers be disjoint.
    if let Some(Event::Scratch { release, .. }) = plan.events.last_mut() {
        *release = Release::Before(next);
    }
    scratch(plan, 0);
    Ok(immutable(text.len() as u64))
}
fn match_value(
    plan: &mut Contract,
    subject: &Subject<'_>,
    found: &Found,
) -> Result<Holding, NativeError> {
    let group = |plan: &mut Contract, range: &Option<Range<usize>>| match range {
        Some(range) => text_holding(plan, &subject.units()[range.clone()]),
        None => Ok(immutable(0)),
    };
    let mut groups = vec![text_holding(plan, &subject.units()[found.range.clone()])?];
    for capture in &found.captures {
        groups.push(group(plan, capture)?);
    }
    let named = if found.named.is_empty() {
        immutable(0)
    } else {
        let mut fields = Vec::new();
        for (name, range) in &found.named {
            fields.push((name.as_str(), group(plan, range)?));
        }
        record(plan, fields)
    };
    let groups = entity(plan, Entity::List, groups);
    let mut fields = vec![
        ("index", integer(&Integer::new(found.range.start))),
        ("groups", groups),
        ("named", named),
    ];
    if subject.regex.flags.has_indices {
        let mut spans = Vec::new();
        for range in
            std::iter::once(Some(&found.range)).chain(found.captures.iter().map(Option::as_ref))
        {
            spans.push(match range {
                None => immutable(0),
                Some(range) => entity(
                    plan,
                    Entity::List,
                    vec![
                        integer(&Integer::new(range.start)),
                        integer(&Integer::new(range.end)),
                    ],
                ),
            });
        }
        let named = if found.named.is_empty() {
            immutable(0)
        } else {
            let lease = plan.events.len();
            scratch(
                plan,
                Cost::OBJECT
                    + subject
                        .capture_names
                        .iter()
                        .map(|(name, _)| Cost::REFERENCE + name.len() as u64)
                        .sum::<u64>(),
            );
            let mut by_name = std::collections::HashMap::new();
            for (name, index) in &subject.capture_names {
                let span = spans[*index];
                if matches!(span.origin, Origin::Fresh(_)) || !by_name.contains_key(name.as_str()) {
                    by_name.insert(name.as_str(), span);
                }
            }
            let output = record(
                plan,
                found
                    .named
                    .iter()
                    .map(|(name, _)| {
                        (
                            name.as_str(),
                            by_name.remove(name.as_str()).unwrap_or(immutable(0)),
                        )
                    })
                    .collect(),
            );
            let release_owner = Owner(plan.events.len() as u32 - 1);
            if let Event::Scratch { release, .. } = &mut plan.events[lease] {
                *release = Release::Before(release_owner);
            }
            output
        };
        let spans = entity(plan, Entity::List, spans);
        let indices = record(plan, vec![("groups", spans), ("named", named)]);
        fields.push(("indices", indices));
    }
    Ok(record(plan, fields))
}

// GetSubstitution's length/content facts, without allocating guest values.
fn replacement_units(
    replacement: &str,
    input: &[u16],
    matches: &[Found],
    counter: &mut WorkCounter,
) -> Result<Vec<u16>, NativeError> {
    let template: Vec<_> = replacement.chars().collect();
    let mut output = ProjectedUnits {
        units: Vec::new(),
        counter,
    };
    let mut end = 0;
    for found in matches {
        output.extend_from_slice(&input[end..found.range.start])?;
        let mut at = 0;
        while at < template.len() {
            let character = template[at];
            let next = template.get(at + 1).copied().filter(|_| character == '$');
            let mut consumed = 2;
            match next {
                Some('$') => output.push(u16::from(b'$'))?,
                Some('&') => output.extend_from_slice(&input[found.range.clone()])?,
                Some('`') => output.extend_from_slice(&input[..found.range.start])?,
                Some('\'') => output.extend_from_slice(&input[found.range.end..])?,
                Some('<') if !found.named.is_empty() && template[at + 2..].contains(&'>') => {
                    let close = at
                        + 2
                        + template[at + 2..]
                            .iter()
                            .position(|ch| *ch == '>')
                            .unwrap_or(0);
                    let name: String = template[at + 2..close].iter().collect();
                    if let Some((_, Some(range))) =
                        found.named.iter().find(|(group, _)| *group == name)
                    {
                        output.extend_from_slice(&input[range.clone()])?;
                    }
                    consumed = close - at + 1;
                }
                Some(digit) if digit.is_ascii_digit() => {
                    let first = digit.to_digit(10).unwrap_or(0) as usize;
                    let both = template
                        .get(at + 2)
                        .and_then(|digit| digit.to_digit(10))
                        .map(|next| first * 10 + next as usize)
                        .filter(|group| (1..=found.captures.len()).contains(group));
                    if let Some(group) = both
                        .or(Some(first).filter(|group| (1..=found.captures.len()).contains(group)))
                    {
                        if let Some(range) = &found.captures[group - 1] {
                            output.extend_from_slice(&input[range.clone()])?;
                        }
                        consumed += usize::from(both.is_some());
                    } else {
                        output.push(u16::from(b'$'))?;
                        consumed = 1;
                    }
                }
                _ => {
                    output.extend_from_slice(character.encode_utf16(&mut [0; 2]))?;
                    consumed = 1;
                }
            }
            at += consumed;
        }
        end = found.range.end;
    }
    output.extend_from_slice(&input[end..])?;
    Ok(output.units)
}

struct ProjectedUnits<'a> {
    units: Vec<u16>,
    counter: &'a mut WorkCounter,
}
impl ProjectedUnits<'_> {
    fn extend_from_slice(&mut self, units: &[u16]) -> Result<(), NativeError> {
        self.counter.spend(units.len() as u64)?;
        self.units.extend_from_slice(units);
        Ok(())
    }
    fn push(&mut self, unit: u16) -> Result<(), NativeError> {
        self.extend_from_slice(&[unit])
    }
}
