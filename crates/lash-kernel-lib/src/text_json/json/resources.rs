//! JSON semantic queries share token and layout rules, but never allocate a
//! guest value. Entity events are authored from the parsed input tree.
mod numbers;
mod reader;
mod writer;
use super::*;
use lash_kernel_doc::{
    LogicalHeldV1 as Cost, NativeResourceContract as Contract, NativeResourceEntity as Entity,
    NativeResourceEvent as Event, NativeResourceExit as Exit, NativeResourceHolding as Holding,
    NativeResourceOrigin as Origin, NativeResourceOwner as Owner, NativeResourceRelease as Release,
};
use numbers::convert_number;

fn overflow() -> NativeError {
    NativeError::Malformed("native logical cost overflow")
}
fn immutable(bytes: u64) -> Holding {
    Holding {
        bytes,
        origin: Origin::Immutable,
    }
}
fn text(bytes: usize) -> Holding {
    immutable(Cost::REFERENCE + bytes as u64)
}
fn numeric(number: Number) -> Holding {
    immutable(
        Cost::REFERENCE
            + match number {
                Number::Int(n) => n.bits().div_ceil(8),
                Number::Float(_) => 0,
            },
    )
}
struct Graph {
    events: Vec<Event>,
    converted: u64,
}
impl Graph {
    fn node(
        &mut self,
        json: &Json<'_>,
        numbers: &Type,
        policy: NumberPolicy,
        maps: bool,
        ieee: bool,
    ) -> Result<Holding, NativeError> {
        Ok(match json {
            Json::Null | Json::Bool(_) => immutable(Cost::REFERENCE),
            Json::Text(s) => text(s.len()),
            Json::Number(token) => {
                let value = if ieee {
                    token.parse::<f64>().map_err(|_| mismatch())?;
                    immutable(Cost::REFERENCE)
                } else {
                    numeric(convert_number(token, numbers, policy)?)
                };
                self.converted = self
                    .converted
                    .checked_add(value.bytes)
                    .ok_or_else(overflow)?;
                value
            }
            Json::Array(values) => {
                let members = values
                    .iter()
                    .map(|v| self.node(v, numbers, policy, maps, ieee))
                    .collect::<Result<_, _>>()?;
                self.entity(Entity::List, members)?
            }
            Json::Object(fields) => {
                let mut names = 0_u64;
                let mut members = vec![];
                for (key, value) in fields {
                    if maps {
                        members.push(text(key.len()));
                    } else {
                        names = names.checked_add(key.len() as u64).ok_or_else(overflow)?;
                    }
                    members.push(self.node(value, numbers, policy, maps, ieee)?);
                }
                self.entity(
                    if maps {
                        Entity::Map
                    } else {
                        Entity::Record {
                            field_name_bytes: names,
                        }
                    },
                    members,
                )?
            }
        })
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
}
fn refused(error: NativeError) -> Result<Contract, NativeError> {
    match error {
        NativeError::Refused { kind, message } => {
            Contract::refused(kind, &message).ok_or_else(overflow)
        }
        other => Err(other),
    }
}
fn parse_contract(args: Args<'_>, op: &str) -> Result<Contract, NativeError> {
    let mut graph = Graph {
        events: vec![],
        converted: 0,
    };
    let mut input_bytes = 0_u64;
    let answer = (|| {
        let maps = if op == "decode" {
            match text_arg(&args, 3)?.as_ref() {
                "map" => true,
                "record" => false,
                _ => return Err(raise("type_error", "expected record or map objects")),
            }
        } else {
            false
        };
        let input = text_arg(&args, 0)?;
        input_bytes = input.len() as u64;
        let json = reader::read(&input, args.limits().document_depth())?;
        let (numbers, policy) = if op == "parse_binary64" {
            (Type::Float, NumberPolicy::Float)
        } else {
            let numbers = match text_arg(&args, 1)?.as_ref() {
                "int" => Type::Int,
                "float" => Type::Float,
                "number" => Type::Number,
                _ => return Err(raise("type_error", "expected int, float or number")),
            };
            let policy = match text_arg(&args, 2)?.as_ref() {
                "by_spelling" => NumberPolicy::BySpelling,
                "float" => NumberPolicy::Float,
                _ => return Err(raise("type_error", "expected a number policy")),
            };
            (numbers, policy)
        };
        graph.node(&json, &numbers, policy, maps, op == "parse_binary64")
    })();
    let mut contract = match answer {
        Ok(output) => Contract {
            exit: Exit::Returned,
            output,
            events: vec![],
        },
        Err(error) => {
            for event in &mut graph.events {
                if let Event::Entity { release, .. } = event {
                    *release = Some(Release::Exit);
                }
            }
            refused(error)?
        }
    };
    contract.events = graph.events;
    // At most one parsed node and one construction continuation per source
    // byte, plus decoded text/token payload and converted number occurrences.
    // Decimal digits need at most three bytes per integer magnitude byte;
    // conversion can hold those digits and the integer simultaneously.
    let scratch = input_bytes
        .checked_mul(2 * Cost::REFERENCE + 2)
        .and_then(|n| n.checked_add(graph.converted.checked_mul(4)?))
        .ok_or_else(overflow)?;
    crate::text_json::resources::scratch(&mut contract, scratch)?;
    Ok(contract)
}
fn workspace(
    args: Args<'_>,
    value: &Value,
    active: &mut Vec<ArenaHandle>,
    depth: usize,
) -> Result<u64, NativeError> {
    if depth > args.limits().document_depth().min(64) {
        return Ok(0);
    }
    let mut bytes = Cost::native_local(args, value)?
        .checked_add(4 * Cost::REFERENCE)
        .ok_or_else(overflow)?;
    if matches!(value, Value::Tuple(_) | Value::List(_)) {
        if let Some(id) = value.handle() {
            if active.contains(&id) {
                return Ok(bytes);
            }
            active.push(id);
        }
        for chunk in args.sequence(value)?.ok_or_else(overflow)?.chunks() {
            for value in chunk? {
                bytes = bytes
                    .checked_add(workspace(args, value, active, depth + 1)?)
                    .ok_or_else(overflow)?;
            }
        }
        active.pop();
    } else if matches!(value, Value::Record(_) | Value::Map(_)) {
        let id = value.handle().ok_or_else(overflow)?;
        if active.contains(&id) {
            return Ok(bytes);
        }
        active.push(id);
        for chunk in args.table(value)?.ok_or_else(overflow)?.chunks() {
            for value in chunk? {
                bytes = bytes
                    .checked_add(workspace(args, value, active, depth + 1)?)
                    .ok_or_else(overflow)?;
            }
        }
        active.pop();
    }
    Ok(bytes)
}
fn writer(args: Args<'_>, op: &str) -> Result<Contract, NativeError> {
    let mut out = writer::Counter {
        bytes: 0,
        args: &args,
        layout: None,
        verbatim_kinds: 0,
        verbatim_field: None,
        encoding: None,
    };
    let answer = (|| {
        if op == "render" {
            let kinds = arg(args.values(), 1)?;
            let (Value::Set(_), Some(table)) = (kinds, args.table(kinds)?) else {
                return Err(raise("type_error", "JSON verbatim kinds must be a set"));
            };
            for chunk in table.chunks() {
                for pair in chunk?.as_chunks::<2>().0 {
                    let Some(name) = args.text(&pair[0])? else {
                        return Err(raise("type_error", "JSON verbatim kinds must be text"));
                    };
                    out.verbatim_kinds |= kind_bit(&name.whole()?);
                }
            }
            out.verbatim_field = match arg(args.values(), 2)? {
                Value::Absent => None,
                value @ (Value::Text(_) | Value::LongText(_)) => {
                    Some(text_copy(&args, value)?.into_owned())
                }
                _ => {
                    return Err(raise(
                        "type_error",
                        "JSON verbatim field must be text or absent",
                    ));
                }
            };
            out.layout = Some(DecimalLayout::read(arg(args.values(), 3)?, &args)?);
        } else if op == "encode" {
            out.layout = Some(DecimalLayout::read(arg(args.values(), 1)?, &args)?);
            let value = arg(args.values(), 2)?;
            let (Value::Tuple(_), Some(style)) = (value, args.sequence(value)?) else {
                return Err(raise("type_error", "expected JSON style tuple"));
            };
            if style.len() != 6 {
                return Err(raise("type_error", "expected six JSON style fields"));
            }
            let values = (0..6)
                .map(|at| style.get(at))
                .collect::<Result<Vec<_>, _>>()?;
            let [
                Some(Value::Bool(ascii)),
                Some(item),
                Some(key),
                Some(Value::Int(width)),
                Some(Value::Bool(fractional)),
                Some(excluded),
            ] = values.as_slice()
            else {
                return Err(raise("type_error", "invalid JSON style fields"));
            };
            if !(0..=20).contains(width) {
                return Err(raise("number_range", "exponent width must be in 0..20"));
            }
            let (Value::Tuple(_), Some(excluded)) = (excluded, args.sequence(excluded)?) else {
                return Err(raise("type_error", "expected excluded record fields"));
            };
            if excluded.len() != 3 {
                return Err(raise("type_error", "expected three excluded record fields"));
            }
            let excluded = (0..3)
                .map(|at| {
                    let field = excluded
                        .get(at)?
                        .ok_or_else(|| raise("type_error", "missing excluded field"))?;
                    Ok(text_copy(&args, field)?.into_owned())
                })
                .collect::<Result<Vec<_>, NativeError>>()?;
            out.encoding = Some(Encoding {
                ascii: *ascii,
                item_separator: text_copy(&args, item)?.into_owned(),
                key_separator: text_copy(&args, key)?.into_owned(),
                exponent_digits: *width as usize,
                fractional: *fractional,
                excluded,
            });
        }
        writer::write(
            arg(args.values(), 0)?,
            &mut vec![],
            0,
            op != "stringify",
            &mut out,
        )
    })();
    let mut contract = match answer {
        Ok(()) => Contract::value(
            Exit::Returned,
            Cost::REFERENCE + out.bytes as u64,
            Origin::Immutable,
        ),
        Err(error) => refused(error)?,
    };
    let mut bytes = out.bytes as u64;
    for value in args.values() {
        bytes = bytes
            .checked_add(workspace(args, value, &mut vec![], 0)?)
            .ok_or_else(overflow)?;
    }
    crate::text_json::resources::scratch(&mut contract, bytes)?;
    Ok(contract)
}
pub(crate) fn evaluate(args: Args<'_>, op: &str) -> Result<Contract, NativeError> {
    match op {
        "parse" | "parse_binary64" | "decode" => parse_contract(args, op),
        _ => writer(args, op),
    }
}
