//! WHATWG URL parsing and form-query codecs over branded kernel records.
//!
//! Native calls allocate new data and never mutate their inputs. The dialect
//! owns setters and the stable live URLSearchParams alias.
use lash_kernel_doc::{
    ErrorValue, FunctionId, FunctionRegistry, NativeCall, NativeError, NativeFunction, Object,
    RegistryError, Value, parse_definition,
};
use std::{collections::BTreeMap, sync::Arc};
use url::{Url, form_urlencoded};

/// Registers parsing, component replacement and form-query codecs.
#[expect(
    clippy::expect_used,
    reason = "literal definitions are valid kernel text"
)]
pub fn register(
    registry: &mut FunctionRegistry,
) -> Result<BTreeMap<String, FunctionId>, RegistryError> {
    let mut result = BTreeMap::new();
    for (name, signature, charge, op) in [
        (
            "url.whatwg.parse",
            "(input: Text, base: Any) -> Any",
            "sum(32, size(input), deep(base), deep(result))",
            Op::Parse,
        ),
        (
            "url.whatwg.update",
            "(href: Text, field: Text, value: Text) -> Any",
            "sum(32, size(href), size(field), size(value), deep(result))",
            Op::Update,
        ),
        (
            "url.whatwg.query_parse",
            "(input: Text) -> List(Any)",
            "sum(8, size(input), deep(result))",
            Op::QueryParse,
        ),
        (
            "url.whatwg.query_stringify",
            "(pairs: List(Any)) -> Text",
            "sum(8, deep(pairs), size(result))",
            Op::QueryStringify,
        ),
    ] {
        let definition = parse_definition(&format!(
            "function {name}{signature}\nkernel 1\nerrors \"type_error\"\ncharge {charge}\nnative\n"
        ))
        .expect("URL definition");
        result.insert(
            name.to_owned(),
            registry.register(definition, Some(Arc::new(op)))?,
        );
    }
    Ok(result)
}
#[derive(Clone, Copy)]
enum Op {
    Parse,
    Update,
    QueryParse,
    QueryStringify,
}
fn raise(message: &str) -> NativeError {
    NativeError::Raised(ErrorValue::new("type_error", message))
}
fn text<'a>(call: &'a NativeCall<'_>, index: usize) -> Result<&'a str, NativeError> {
    match call.args.get(index) {
        Some(Value::Text(value)) => Ok(value),
        _ => Err(raise("Expected text")),
    }
}
fn allocate(call: &mut NativeCall<'_>, object: Object) -> Result<Value, NativeError> {
    let list = matches!(object, Object::List(_));
    let id = call.heap.allocate(object)?;
    Ok(if list {
        Value::List(id)
    } else {
        Value::Record(id)
    })
}
fn pairs(call: &mut NativeCall<'_>, input: &str) -> Result<Value, NativeError> {
    let mut result = Vec::new();
    for (name, value) in form_urlencoded::parse(input.trim_start_matches('?').as_bytes()) {
        result.push(allocate(
            call,
            Object::List(vec![Value::text(name), Value::text(value)]),
        )?);
    }
    allocate(call, Object::List(result))
}
fn record(call: &mut NativeCall<'_>, url: &Url) -> Result<Value, NativeError> {
    let fields = [
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
            url.query().map_or_else(String::new, |q| {
                if q.is_empty() {
                    String::new()
                } else {
                    format!("?{q}")
                }
            }),
        ),
        (
            "hash",
            url.fragment().map_or_else(String::new, |f| {
                if f.is_empty() {
                    String::new()
                } else {
                    format!("#{f}")
                }
            }),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), Value::text(value)))
    .collect();
    let state = allocate(call, Object::Record(fields))?;
    let decoded = pairs(call, url.query().unwrap_or(""))?;
    let params = allocate(
        call,
        Object::Record(vec![
            ("brand".into(), Value::text("url.search_params")),
            ("pairs".into(), decoded),
            ("owner".into(), state.clone()),
        ]),
    )?;
    allocate(
        call,
        Object::Record(vec![
            ("brand".into(), Value::text("url.whatwg")),
            ("state".into(), state),
            ("searchParams".into(), params),
        ]),
    )
}
impl NativeFunction for Op {
    fn call(&self, mut call: NativeCall<'_>) -> Result<Value, NativeError> {
        match self {
            Self::Parse => {
                let input = text(&call, 0)?;
                let url = match call.args.get(1) {
                    Some(Value::Absent) => Url::parse(input),
                    Some(Value::Text(base)) => Url::parse(base).and_then(|base| base.join(input)),
                    _ => return Err(raise("Expected text or absent base")),
                }
                .map_err(|_| raise("Invalid URL"))?;
                record(&mut call, &url)
            }
            Self::Update => {
                let mut url = Url::parse(text(&call, 0)?).map_err(|_| raise("Invalid URL"))?;
                let field = text(&call, 1)?;
                let value = text(&call, 2)?;
                match field {
                    "href" => url = url.join(value).map_err(|_| raise("Invalid URL"))?,
                    "protocol" => {
                        let _ = url.set_scheme(value.trim_end_matches(':'));
                    }
                    "username" => {
                        let _ = url.set_username(value);
                    }
                    "password" => {
                        let _ = url.set_password(Some(value));
                    }
                    "hostname" => {
                        let _ = url.set_host(Some(value));
                    }
                    "host" => {
                        let authority = format!("{}://{value}/", url.scheme());
                        if let Ok(parsed) = Url::parse(&authority) {
                            let _ = url.set_host(parsed.host_str());
                            let _ = url.set_port(parsed.port());
                        }
                    }
                    "port" => {
                        if value.is_empty() {
                            let _ = url.set_port(None);
                        } else if let Ok(port) = value.parse() {
                            let _ = url.set_port(Some(port));
                        }
                    }
                    "pathname" => url.set_path(value),
                    "search" => url.set_query(if value.is_empty() {
                        None
                    } else {
                        Some(value.trim_start_matches('?'))
                    }),
                    "hash" => url.set_fragment(if value.is_empty() {
                        None
                    } else {
                        Some(value.trim_start_matches('#'))
                    }),
                    _ => return Err(raise("Unsupported URL field")),
                }
                record(&mut call, &url)
            }
            Self::QueryParse => {
                let input = text(&call, 0)?.to_owned();
                pairs(&mut call, &input)
            }
            Self::QueryStringify => {
                let Some(Value::List(id)) = call.args.first() else {
                    return Err(raise("Expected pair list"));
                };
                let mut serializer = form_urlencoded::Serializer::new(String::new());
                for index in 0..call.heap.len(*id) {
                    let Value::List(pair) = call
                        .heap
                        .list_get(*id, index)
                        .ok_or_else(|| raise("Missing pair"))?
                    else {
                        return Err(raise("Expected pair"));
                    };
                    let Value::Text(name) = call
                        .heap
                        .list_get(pair, 0)
                        .ok_or_else(|| raise("Missing name"))?
                    else {
                        return Err(raise("Expected name"));
                    };
                    let Value::Text(value) = call
                        .heap
                        .list_get(pair, 1)
                        .ok_or_else(|| raise("Missing value"))?
                    else {
                        return Err(raise("Expected value"));
                    };
                    serializer.append_pair(&name, &value);
                }
                Ok(Value::text(serializer.finish()))
            }
        }
    }
}
