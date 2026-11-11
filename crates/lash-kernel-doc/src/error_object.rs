//! One construction rule for boundary data and live guest error objects.

use crate::Datum;
use crate::{Args, NativeError, NewObject, Nursery, Value};

pub const RAW: &str = "__lash_raw";

/// A live error payload produced by the shared object constructor.
#[derive(Clone, Copy, Debug)]
pub struct ErrorObjectValue(Value);

impl ErrorObjectValue {
    pub fn value(self) -> Value {
        self.0
    }
}

type Fields<V> = Vec<(String, V)>;

pub(crate) trait Builder {
    type Value: Clone;
    type Error;

    fn fields(&self, value: &Self::Value) -> Result<Option<Fields<Self::Value>>, Self::Error>;
    fn is_text(&self, value: &Self::Value) -> Result<bool, Self::Error>;
    fn record(&mut self, fields: Vec<(String, Self::Value)>) -> Result<Self::Value, Self::Error>;
}

fn take<V>(fields: &mut Vec<(String, V)>, key: &str) -> Option<V> {
    fields
        .iter()
        .position(|(name, _)| name == key)
        .map(|at| fields.remove(at).1)
}

pub(crate) fn construct<B: Builder>(
    builder: &mut B,
    kind: B::Value,
    message: B::Value,
    data: B::Value,
) -> Result<B::Value, B::Error> {
    let mut fields = builder.fields(&data)?.unwrap_or_default();
    let mut canonical = false;
    if fields.iter().any(|(name, _)| name == RAW)
        && let Some((_, value)) = fields.iter().find(|(name, _)| name == "properties")
        && let Some(properties) = builder.fields(value)?
    {
        canonical = true;
        let mut complete = true;
        for name in ["code", "message"] {
            complete &= match properties.iter().find(|(key, _)| key == name) {
                Some((_, value)) => builder.is_text(value)?,
                None => false,
            };
        }
        let hidden = match fields.iter().find(|(name, _)| name == "nonEnumerable") {
            Some((_, value)) => builder.fields(value)?.unwrap_or_default(),
            None => Vec::new(),
        };
        if complete
            && hidden
                .iter()
                .all(|(name, _)| name != "code" && name != "message")
        {
            return Ok(data);
        }
    }
    let mut properties = match take(&mut fields, "properties") {
        Some(value) => builder.fields(&value)?.unwrap_or_default(),
        None => Vec::new(),
    };
    let retained = take(&mut fields, RAW);
    let raw = if canonical {
        retained.unwrap_or_else(|| data.clone())
    } else {
        data.clone()
    };
    let code = take(&mut properties, "code").or_else(|| {
        fields
            .iter()
            .find(|(name, _)| name == "code")
            .map(|(_, value)| value.clone())
    });
    let code = match code {
        Some(code) if builder.is_text(&code)? => code,
        _ => kind,
    };
    take(&mut properties, "message");
    properties.insert(0, ("message".into(), message));
    properties.insert(0, ("code".into(), code));
    let mut hidden = match take(&mut fields, "nonEnumerable") {
        Some(value) => builder.fields(&value)?.unwrap_or_default(),
        None => Vec::new(),
    };
    hidden.retain(|(name, _)| name != "code" && name != "message");
    fields.push(("properties".into(), builder.record(properties)?));
    fields.push(("nonEnumerable".into(), builder.record(hidden)?));
    fields.push((RAW.into(), raw));
    builder.record(fields)
}

pub(crate) struct DatumBuilder;

impl Builder for DatumBuilder {
    type Value = Datum;
    type Error = std::convert::Infallible;

    fn fields(&self, value: &Datum) -> Result<Option<Vec<(String, Datum)>>, Self::Error> {
        Ok(match value {
            Datum::Record(fields) => Some(fields.clone()),
            _ => None,
        })
    }

    fn is_text(&self, value: &Datum) -> Result<bool, Self::Error> {
        Ok(matches!(value, Datum::Text(_)))
    }

    fn record(&mut self, fields: Vec<(String, Datum)>) -> Result<Datum, Self::Error> {
        Ok(Datum::Record(fields))
    }
}

pub fn native<'call>(
    args: Args<'call>,
    new: &mut Nursery<'call>,
    kind: Value,
    message: Value,
    data: Value,
) -> Result<ErrorObjectValue, NativeError> {
    construct(&mut NativeBuilder { args, new }, kind, message, data).map(ErrorObjectValue)
}

/// Original failure data, without the object's property metadata.
pub fn raw_value(args: Args<'_>, object: &Value) -> Result<Value, NativeError> {
    native_fields(args, object)?
        .and_then(|fields| {
            fields
                .into_iter()
                .find(|(name, _)| name == RAW)
                .map(|(_, value)| value)
        })
        .ok_or(NativeError::Malformed(
            "an error object has no raw failure data",
        ))
}

struct NativeBuilder<'new, 'call> {
    args: Args<'call>,
    new: &'new mut Nursery<'call>,
}

impl Builder for NativeBuilder<'_, '_> {
    type Value = Value;
    type Error = NativeError;

    fn fields(&self, value: &Value) -> Result<Option<Vec<(String, Value)>>, NativeError> {
        if let Some(handle) = value.handle()
            && let Some((_, NewObject::Record(fields))) =
                self.new.built().iter().find(|(id, _)| *id == handle)
        {
            return fields
                .iter()
                .map(|(key, value)| {
                    let name = match key {
                        Value::Text(text) => text.as_str().to_owned(),
                        Value::LongText(handle) => {
                            match self.new.built().iter().find(|(id, _)| id == handle) {
                                Some((_, NewObject::LongText { text, .. })) => text.to_string(),
                                _ => self
                                    .args
                                    .text(key)?
                                    .ok_or(NativeError::Malformed("a record key is not text"))?
                                    .whole()?
                                    .into_owned(),
                            }
                        }
                        _ => return Err(NativeError::Malformed("a record key is not text")),
                    };
                    Ok((name, *value))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some);
        }
        native_fields(self.args, value)
    }

    fn is_text(&self, value: &Value) -> Result<bool, NativeError> {
        Ok(matches!(value, Value::Text(_) | Value::LongText(_)))
    }

    fn record(&mut self, fields: Vec<(String, Value)>) -> Result<Value, NativeError> {
        let names = fields.iter().map(|(name, _)| name.len() as u64).sum();
        self.new.reserve(fields.len() as u64, names)?;
        let fields = fields
            .into_iter()
            .map(|(name, value)| (self.new.text(&name), value))
            .collect();
        Ok(self.new.allocate(NewObject::Record(fields)))
    }
}

fn native_fields(
    args: Args<'_>,
    value: &Value,
) -> Result<Option<Vec<(String, Value)>>, NativeError> {
    if !matches!(value, Value::Record(_)) {
        return Ok(None);
    }
    let table = args
        .table(value)?
        .ok_or(NativeError::Malformed("a record has no table"))?;
    let mut fields = Vec::with_capacity(table.len() as usize);
    for chunk in table.chunks() {
        for [key, value] in chunk?.as_chunks::<2>().0 {
            let name = args
                .text(key)?
                .ok_or(NativeError::Malformed("a record key is not text"))?
                .whole()?
                .into_owned();
            fields.push((name, *value));
        }
    }
    Ok(Some(fields))
}

use crate::{
    LogicalHeldV1 as Cost, NativeResourceContract as Contract, NativeResourceEntity as Entity,
    NativeResourceEvent as Event, NativeResourceExit as Exit, NativeResourceHolding as Holding,
    NativeResourceOrigin as Origin, NativeResourceOwner as Owner,
};

#[derive(Clone)]
struct ResourceValue {
    source: Option<Value>,
    text: bool,
    holding: Holding,
}

struct ResourceBuilder<'call> {
    args: Option<Args<'call>>,
    events: Vec<Event>,
}

fn overflow() -> NativeError {
    NativeError::Malformed("error object logical cost overflow")
}

impl ResourceValue {
    fn source(args: Args<'_>, value: Value) -> Result<Self, NativeError> {
        Ok(Self {
            source: Some(value),
            text: matches!(value, Value::Text(_) | Value::LongText(_)),
            holding: Contract::returned(args, &value)?.output,
        })
    }

    fn literal(text: Option<&str>) -> Self {
        Self {
            source: None,
            text: text.is_some(),
            holding: Holding {
                bytes: Cost::REFERENCE + text.map_or(0, |text| text.len() as u64),
                origin: Origin::Immutable,
            },
        }
    }
}

impl Builder for ResourceBuilder<'_> {
    type Value = ResourceValue;
    type Error = NativeError;

    fn fields(
        &self,
        value: &ResourceValue,
    ) -> Result<Option<Vec<(String, ResourceValue)>>, NativeError> {
        let (Some(args), Some(value)) = (self.args, value.source) else {
            return Ok(None);
        };
        native_fields(args, &value)?
            .map(|fields| {
                fields
                    .into_iter()
                    .map(|(name, value)| Ok((name, ResourceValue::source(args, value)?)))
                    .collect()
            })
            .transpose()
    }

    fn is_text(&self, value: &ResourceValue) -> Result<bool, NativeError> {
        Ok(value.text)
    }

    fn record(
        &mut self,
        fields: Vec<(String, ResourceValue)>,
    ) -> Result<ResourceValue, NativeError> {
        let owner = Owner(self.events.len().try_into().map_err(|_| overflow())?);
        let names = fields
            .iter()
            .try_fold(0_u64, |bytes, (name, _)| {
                bytes.checked_add(name.len() as u64)
            })
            .ok_or_else(overflow)?;
        self.events.push(Event::Entity {
            owner,
            kind: Entity::Record {
                field_name_bytes: names,
            },
            release: None,
            members: fields.into_iter().map(|(_, value)| value.holding).collect(),
        });
        Ok(ResourceValue {
            source: None,
            text: false,
            holding: Holding {
                bytes: Cost::REFERENCE,
                origin: Origin::Fresh(owner),
            },
        })
    }
}

fn contract(
    mut builder: ResourceBuilder<'_>,
    kind: ResourceValue,
    message: ResourceValue,
    data: ResourceValue,
    payload_bytes: u64,
    exit: Exit,
) -> Result<Contract, NativeError> {
    let object = construct(&mut builder, kind, message, data)?;
    let bytes = Cost::REFERENCE
        .checked_add(payload_bytes)
        .and_then(|bytes| bytes.checked_add(object.holding.bytes))
        .ok_or_else(overflow)?;
    Ok(Contract {
        exit,
        events: builder.events,
        output: Holding {
            bytes,
            origin: Origin::Immutable,
        },
    })
}

pub fn native_contract(
    args: Args<'_>,
    kind: Value,
    message: Value,
    data: Value,
) -> Result<Contract, NativeError> {
    let payload_bytes = args
        .text(&kind)?
        .ok_or_else(overflow)?
        .len()
        .checked_add(args.text(&message)?.ok_or_else(overflow)?.len())
        .ok_or_else(overflow)?;
    contract(
        ResourceBuilder {
            args: Some(args),
            events: Vec::new(),
        },
        ResourceValue::source(args, kind)?,
        ResourceValue::source(args, message)?,
        ResourceValue::source(args, data)?,
        payload_bytes,
        Exit::Returned,
    )
}

pub(crate) fn refused_contract(
    mut events: Vec<Event>,
    kind: &str,
    message: &str,
) -> Option<Contract> {
    for event in &mut events {
        if let Event::Entity { release, .. } = event {
            *release = Some(crate::NativeResourceRelease::Exit);
        }
    }
    let payload_bytes = (kind.len() as u64).checked_add(message.len() as u64)?;
    contract(
        ResourceBuilder { args: None, events },
        ResourceValue::literal(Some(kind)),
        ResourceValue::literal(Some(message)),
        ResourceValue::literal(None),
        payload_bytes,
        Exit::Raised,
    )
    .ok()
}
