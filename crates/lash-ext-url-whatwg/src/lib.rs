//! WHATWG URL parsing and form-query codecs over branded kernel records.
//!
//! Native calls allocate new data and never mutate their inputs. The dialect
//! owns setters and the stable live URLSearchParams alias.
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::NativeCall;
use lash_kernel_doc::NativeError;
use lash_kernel_doc::NativeFunction;
use lash_kernel_doc::NewObject;
use lash_kernel_doc::RegistryError;
use lash_kernel_doc::Value;
use lash_kernel_doc::parse_definition;
use lash_vocabulary::names::FunctionId;
use lash_vocabulary::types::RecordType;
use lash_vocabulary::types::RecordTypeField;
use lash_vocabulary::types::Type;
use std::{borrow::Cow, collections::BTreeMap, sync::Arc};
use url::{Url, form_urlencoded};

/// Declares parsing, component replacement and form-query codecs in a declarations registry.
/// Install natives through [`implement`].
#[expect(
    clippy::expect_used,
    reason = "literal definitions are valid kernel text"
)]
pub fn register(
    registry: &mut FunctionRegistry,
) -> Result<BTreeMap<String, FunctionId>, RegistryError> {
    let mut result = BTreeMap::new();
    for (signature, charge, op) in [
        (
            "(input: Text, base: Any) -> Any",
            "sum(32, size(input), deep(base), deep(result))",
            Operation::Parse,
        ),
        (
            "(href: Text, field: Text, value: Text) -> Any",
            "sum(32, size(href), size(field), size(value), deep(result))",
            Operation::Update,
        ),
        (
            "(input: Text) -> List(Any)",
            "sum(8, size(input), deep(result))",
            Operation::QueryParse,
        ),
        (
            "(pairs: List(Any)) -> Text",
            "sum(8, deep(pairs), size(result))",
            Operation::QueryStringify,
        ),
    ] {
        let name = op.name();
        let mut definition = parse_definition(&format!(
            "function {name}{signature}\nkernel 1\nerrors \"type_error\"\ncharge {charge}\nnative\n"
        ))
        .expect("URL definition");
        if matches!(op, Operation::Parse | Operation::Update) {
            definition.signature.result = url_type();
        }
        result.insert(name.to_owned(), registry.register(definition, None)?);
    }
    Ok(result)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Parse,
    Update,
    QueryParse,
    QueryStringify,
}
impl Operation {
    pub const ALL: [Self; 4] = [
        Self::Parse,
        Self::Update,
        Self::QueryParse,
        Self::QueryStringify,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Parse => "url.whatwg.parse",
            Self::Update => "url.whatwg.update",
            Self::QueryParse => "url.whatwg.query_parse",
            Self::QueryStringify => "url.whatwg.query_stringify",
        }
    }
}

fn record_type<const N: usize>(fields: [(&str, Type); N]) -> Type {
    Type::Record(RecordType {
        fields: fields
            .into_iter()
            .map(|(name, ty)| RecordTypeField {
                name: name.into(),
                ty,
                optional: false,
            })
            .collect(),
        rest: lash_vocabulary::RecordRest::Closed,
    })
}

fn brand_type(ty: &str) -> Type {
    Type::Tuple(vec![
        Type::Enum(vec!["url.whatwg".into()]),
        Type::Enum(vec![ty.into()]),
    ])
}

fn url_type() -> Type {
    let state = Type::Record(RecordType {
        fields: vec![],
        rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Text)),
    });
    let params = record_type([
        ("__lash_brand", brand_type("url_search_params")),
        ("__lash_built", Type::Bytes),
        (
            "pairs",
            Type::List(Box::new(Type::List(Box::new(Type::Text)))),
        ),
        ("owner", state.clone()),
    ]);
    record_type([
        ("__lash_brand", brand_type("url")),
        ("__lash_built", Type::Bytes),
        ("state", state),
        ("searchParams", params),
    ])
}

fn raise(message: &'static str) -> NativeError {
    NativeError::refused("type_error", message)
}
/// Text argument `index` whole, as the heap copied it out: a paged text
/// holds its bytes ([`lash_kernel_doc::Nursery::read_whole`]).
fn text<'call>(
    call: &mut NativeCall<'call, '_>,
    index: usize,
) -> Result<Cow<'call, str>, NativeError> {
    match call.args.values().get(index) {
        Some(value) => match call.args.text(value)? {
            Some(text) => {
                call.new.read_whole(text.len())?;
                text.whole()
            }
            None => Err(raise("Expected text")),
        },
        None => Err(raise("Expected text")),
    }
}
fn pairs(call: &mut NativeCall<'_, '_>, input: &str) -> Value {
    let mut result = Vec::new();
    for (name, value) in form_urlencoded::parse(input.trim_start_matches('?').as_bytes()) {
        let (name, value) = (call.new.text(&name), call.new.text(&value));
        result.push(call.new.allocate(NewObject::List(vec![name, value])));
    }
    call.new.allocate(NewObject::List(result))
}
fn record(call: &mut NativeCall<'_, '_>, url: &Url, built: FunctionId) -> Value {
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
    .map(|(name, value)| (call.new.text(name), call.new.text(&value)))
    .collect();
    let state = call.new.allocate(NewObject::Record(fields));
    let decoded = pairs(call, url.query().unwrap_or(""));
    let new = &mut *call.new;
    let built = new.bytes(built.as_bytes());
    let package = new.text("url.whatwg");
    let params_type = new.text("url_search_params");
    let params_brand = new.allocate(NewObject::Tuple(vec![package, params_type]));
    let params = vec![
        (new.text("__lash_brand"), params_brand),
        (new.text("__lash_built"), built),
        (new.text("pairs"), decoded),
        (new.text("owner"), state),
    ];
    let params = new.allocate(NewObject::Record(params));
    let url_type = new.text("url");
    let url_brand = new.allocate(NewObject::Tuple(vec![package, url_type]));
    let fields = vec![
        (new.text("__lash_brand"), url_brand),
        (new.text("__lash_built"), built),
        (new.text("state"), state),
        (new.text("searchParams"), params),
    ];
    new.allocate(NewObject::Record(fields))
}
struct Function {
    operation: Operation,
    /// This function's identity, which a value it builds holds.
    own: FunctionId,
}

impl NativeFunction for Function {
    fn call(&self, mut call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        match self.operation {
            Operation::Parse => {
                let input = text(&mut call, 0)?;
                let url = match call.args.get(1) {
                    Some(Value::Absent) => Url::parse(&input),
                    Some(Value::Text(_) | Value::LongText(_)) => {
                        let base = text(&mut call, 1)?;
                        Url::parse(&base).and_then(|base| base.join(&input))
                    }
                    _ => return Err(raise("Expected text or absent base")),
                }
                .map_err(|_| raise("Invalid URL"))?;
                Ok(record(&mut call, &url, self.own))
            }
            Operation::Update => {
                let mut url = Url::parse(&text(&mut call, 0)?).map_err(|_| raise("Invalid URL"))?;
                let field = text(&mut call, 1)?;
                let value = text(&mut call, 2)?;
                match field.as_ref() {
                    "href" => url = url.join(&value).map_err(|_| raise("Invalid URL"))?,
                    "protocol" => {
                        let _ = url.set_scheme(value.trim_end_matches(':'));
                    }
                    "username" => {
                        let _ = url.set_username(&value);
                    }
                    "password" => {
                        let _ = url.set_password(Some(&value));
                    }
                    "hostname" => {
                        let _ = url.set_host(Some(&value));
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
                    "pathname" => url.set_path(&value),
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
                Ok(record(&mut call, &url, self.own))
            }
            Operation::QueryParse => {
                let input = text(&mut call, 0)?;
                Ok(pairs(&mut call, &input))
            }
            Operation::QueryStringify => {
                let args = call.args;
                let list = args.get(0).unwrap_or(&Value::Absent);
                let (Value::List(_), Some(list)) = (list, args.sequence(list)?) else {
                    return Err(raise("Expected pair list"));
                };
                let mut serializer = form_urlencoded::Serializer::new(String::new());
                for index in 0..list.len() {
                    let pair = list.get(index)?.ok_or_else(|| raise("Missing pair"))?;
                    let (Value::List(_), Some(pair)) = (pair, args.sequence(pair)?) else {
                        return Err(raise("Expected pair"));
                    };
                    let name = pair.get(0)?.ok_or_else(|| raise("Missing name"))?;
                    let Some(name) = args.text(name)? else {
                        return Err(raise("Expected name"));
                    };
                    let value = pair.get(1)?.ok_or_else(|| raise("Missing value"))?;
                    let Some(value) = args.text(value)? else {
                        return Err(raise("Expected value"));
                    };
                    call.new.read_whole(name.len())?;
                    call.new.read_whole(value.len())?;
                    serializer.append_pair(&name.whole()?, &value.whole()?);
                }
                Ok(call.new.text(&serializer.finish()))
            }
        }
    }
}

/// Adds natives to the registered build definitions this extension implements.
pub fn implement(
    registry: &mut FunctionRegistry,
    functions: impl IntoIterator<Item = FunctionId>,
) -> Result<(), RegistryError> {
    for function in functions {
        let registered = registry
            .get(&function)
            .ok_or(RegistryError::NotRegistered { function })?;
        if let Some(operation) = Operation::ALL
            .into_iter()
            .find(|operation| operation.name() == registered.name())
        {
            registry.implement(
                function,
                Arc::new(Function {
                    operation,
                    own: function,
                }),
            )?;
        }
    }
    Ok(())
}

/// Constructs the implementation of `function`, selected by the build's
/// identity table.
pub fn native(
    name: &str,
    function: FunctionId,
) -> Option<Arc<dyn lash_kernel_doc::NativeFunction>> {
    Operation::ALL
        .into_iter()
        .find(|operation| operation.name() == name)
        .map(|operation| {
            Arc::new(Function {
                operation,
                own: function,
            }) as Arc<dyn lash_kernel_doc::NativeFunction>
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_doc::{Datum, WorkCounter};

    use lash_kernel_state::arena::Bench;

    /// BRAND-ONE: a native constructor returns its canonical package/type
    /// tuple and the identity it is installed under.
    #[test]
    fn brand_one_native_constructor_stamps_its_package_and_identity() {
        let built = FunctionId::from_bytes([0x5a; 32]);
        let mut heap = Bench::new();
        let source = heap.arena_mut().text("https://example.test/?a=1");
        let arguments = [source, Value::Absent];
        let native = native("url.whatwg.parse", built).expect("the native constructor");
        let value = heap
            .call(
                native.as_ref(),
                &arguments,
                u64::MAX,
                &mut WorkCounter::new(None),
            )
            .answer
            .expect("native constructor");
        let Datum::Record(fields) = heap.datum(&value).expect("record data") else {
            panic!("branded record");
        };
        let brand = fields
            .iter()
            .find(|(name, _)| name == "__lash_brand")
            .map(|(_, value)| value);
        assert_eq!(
            brand,
            Some(&Datum::Tuple(vec![
                Datum::Text("url.whatwg".into()),
                Datum::Text("url".into())
            ]))
        );
        let bytes = fields
            .iter()
            .find(|(name, _)| name == "__lash_built")
            .map(|(_, value)| value);
        assert_eq!(
            bytes,
            Some(&Datum::Bytes(lash_kernel_doc::Bytes::new(
                built.as_bytes().to_vec()
            )))
        );
    }
}
