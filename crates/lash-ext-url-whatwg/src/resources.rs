//! Logical shapes and scratch from input facts, independently of nursery execution.
use super::{Function, Operation, raise};
use lash_kernel_doc::{
    Args, LogicalHeldV1 as Cost, NativeError, NativeResourceContract as Contract,
    NativeResourceEntity as Entity, NativeResourceEvent as Event, NativeResourceExit as Exit,
    NativeResourceHolding as Holding, NativeResourceOrigin as Origin, NativeResourceOwner as Owner,
    NativeResourceRelease as Release, Value,
};
use url::{Url, form_urlencoded};

fn value(payload: u64) -> Holding {
    Holding {
        bytes: Cost::REFERENCE + payload,
        origin: Origin::Immutable,
    }
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
            field_name_bytes: fields.iter().map(|(key, _)| key.len() as u64).sum(),
        },
        fields.into_iter().map(|(_, holding)| holding).collect(),
    )
}
fn scratch(plan: &mut Contract, bytes: u64) {
    plan.events.push(Event::Scratch {
        owner: Owner(plan.events.len() as u32),
        bytes,
        release: Release::Exit,
    });
}
fn text(args: Args<'_>, index: usize) -> Result<String, NativeError> {
    args.get(index)
        .map(|value| args.text(value))
        .transpose()?
        .flatten()
        .ok_or_else(|| raise("Expected text"))?
        .whole()
        .map(|text| text.into_owned())
}
fn pairs(plan: &mut Contract, input: &str) -> Holding {
    let mut members = Vec::new();
    let mut peak = 0;
    for (name, part) in form_urlencoded::parse(input.trim_start_matches('?').as_bytes()) {
        peak = peak.max(2 * Cost::REFERENCE + (name.len() + part.len()) as u64);
        members.push(entity(
            plan,
            Entity::List,
            vec![value(name.len() as u64), value(part.len() as u64)],
        ));
    }
    scratch(plan, peak);
    entity(plan, Entity::List, members)
}
fn url_record(plan: &mut Contract, url: &Url) -> Holding {
    // A working URL plus component texts coexist until the record is built.
    let components = [
        ("href", url.as_str().to_owned()),
        ("origin", url.origin().ascii_serialization()),
        ("protocol", format!("{}:", url.scheme())),
        ("username", url.username().to_owned()),
        ("password", url.password().unwrap_or("").to_owned()),
        (
            "host",
            url[url::Position::BeforeHost..url::Position::AfterPort].to_owned(),
        ),
        ("hostname", url.host_str().unwrap_or("").to_owned()),
        (
            "port",
            url.port().map_or_else(String::new, |port| port.to_string()),
        ),
        ("pathname", url.path().to_owned()),
        (
            "search",
            url.query()
                .filter(|part| !part.is_empty())
                .map_or_else(String::new, |part| format!("?{part}")),
        ),
        (
            "hash",
            url.fragment()
                .filter(|part| !part.is_empty())
                .map_or_else(String::new, |part| format!("#{part}")),
        ),
    ];
    scratch(
        plan,
        Cost::REFERENCE
            + url.as_str().len() as u64
            + components
                .iter()
                .map(|(_, part)| Cost::REFERENCE + part.len() as u64)
                .sum::<u64>(),
    );
    let state = record(
        plan,
        components
            .iter()
            .map(|(key, part)| (*key, value(part.len() as u64)))
            .collect(),
    );
    let decoded = pairs(plan, url.query().unwrap_or(""));
    let built = value(32);
    let params = record(
        plan,
        vec![
            ("__lash_brand", value(2 * Cost::REFERENCE + 10 + 17)),
            ("__lash_built", built),
            ("pairs", decoded),
            ("owner", state),
        ],
    );
    record(
        plan,
        vec![
            ("__lash_brand", value(2 * Cost::REFERENCE + 10 + 3)),
            ("__lash_built", built),
            ("state", state),
            ("searchParams", params),
        ],
    )
}

pub(super) fn contract(native: &Function, args: Args<'_>) -> Result<Option<Contract>, NativeError> {
    let mut plan = Contract::value(Exit::Returned, Cost::REFERENCE, Origin::Immutable);
    match evaluate(native.operation, args, &mut plan) {
        Ok(output) => plan.output = output,
        Err(NativeError::Refused { kind, message }) => {
            plan.output = Contract::refused(kind, &message)
                .ok_or(NativeError::Malformed("native logical cost overflow"))?
                .output;
            plan.exit = Exit::Raised;
        }
        Err(error) => return Err(error),
    }
    Ok(Some(plan))
}
fn evaluate(op: Operation, args: Args<'_>, plan: &mut Contract) -> Result<Holding, NativeError> {
    match op {
        Operation::Parse => {
            let input = text(args, 0)?;
            let url = match args.get(1) {
                Some(Value::Absent) => Url::parse(&input),
                Some(Value::Text(_) | Value::LongText(_)) => {
                    let base = text(args, 1)?;
                    Url::parse(&base).and_then(|base| {
                        scratch(plan, Cost::REFERENCE + base.as_str().len() as u64);
                        base.join(&input)
                    })
                }
                _ => return Err(raise("Expected text or absent base")),
            }
            .map_err(|_| raise("Invalid URL"))?;
            Ok(url_record(plan, &url))
        }
        Operation::Update => {
            let mut url = Url::parse(&text(args, 0)?).map_err(|_| raise("Invalid URL"))?;
            scratch(plan, Cost::REFERENCE + url.as_str().len() as u64);
            let field = text(args, 1)?;
            let part = text(args, 2)?;
            match field.as_str() {
                "href" => url = url.join(&part).map_err(|_| raise("Invalid URL"))?,
                "protocol" => {
                    let _ = url.set_scheme(part.trim_end_matches(':'));
                }
                "username" => {
                    let _ = url.set_username(&part);
                }
                "password" => {
                    let _ = url.set_password(Some(&part));
                }
                "hostname" => {
                    let _ = url.set_host(Some(&part));
                }
                "host" => {
                    let authority = format!("{}://{part}/", url.scheme());
                    scratch(plan, Cost::REFERENCE + authority.len() as u64);
                    if let Ok(parsed) = Url::parse(&authority) {
                        scratch(plan, Cost::REFERENCE + parsed.as_str().len() as u64);
                        let _ = url.set_host(parsed.host_str());
                        let _ = url.set_port(parsed.port());
                    }
                }
                "port" => {
                    if part.is_empty() {
                        let _ = url.set_port(None);
                    } else if let Ok(port) = part.parse() {
                        let _ = url.set_port(Some(port));
                    }
                }
                "pathname" => url.set_path(&part),
                "search" => url.set_query((!part.is_empty()).then(|| part.trim_start_matches('?'))),
                "hash" => {
                    url.set_fragment((!part.is_empty()).then(|| part.trim_start_matches('#')))
                }
                _ => return Err(raise("Unsupported URL field")),
            }
            Ok(url_record(plan, &url))
        }
        Operation::QueryParse => Ok(pairs(plan, &text(args, 0)?)),
        Operation::QueryStringify => {
            let list = args.get(0).unwrap_or(&Value::Absent);
            let (Value::List(_), Some(list)) = (list, args.sequence(list)?) else {
                return Err(raise("Expected pair list"));
            };
            let mut serializer = form_urlencoded::Serializer::new(String::new());
            // Each UTF-8 byte expands to at most three ASCII bytes, plus '=' and '&'.
            let mut bound = 0;
            scratch(plan, Cost::REFERENCE);
            for index in 0..list.len() {
                let pair = list.get(index)?.ok_or_else(|| raise("Missing pair"))?;
                let (Value::List(_), Some(pair)) = (pair, args.sequence(pair)?) else {
                    return Err(raise("Expected pair"));
                };
                let name = pair.get(0)?.ok_or_else(|| raise("Missing name"))?;
                let name = args.text(name)?.ok_or_else(|| raise("Expected name"))?;
                let part = pair.get(1)?.ok_or_else(|| raise("Missing value"))?;
                let part = args.text(part)?.ok_or_else(|| raise("Expected value"))?;
                let addition = name
                    .len()
                    .checked_add(part.len())
                    .and_then(|n| n.checked_mul(3))
                    .and_then(|n| n.checked_add(2))
                    .ok_or(NativeError::Malformed("native logical cost overflow"))?;
                bound = u64::checked_add(bound, addition)
                    .ok_or(NativeError::Malformed("native logical cost overflow"))?;
                scratch(plan, addition);
                serializer.append_pair(&name.whole()?, &part.whole()?);
            }
            let output = serializer.finish();
            debug_assert!(output.len() as u64 <= bound);
            Ok(value(output.len() as u64))
        }
    }
}
