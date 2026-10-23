//! Frozen callable environments. First visits are inline nodes; references
//! name earlier nodes in the deterministic depth-first walk, rooted at zero.
use crate::{Datum, FunctionRepresentation, FunctionValue, FunctionValueRefusal};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Data within a frozen environment, including recursive callable references.
/// References have meaning only within the enclosing FunctionValue graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaptureValue {
    Data(Datum),
    Function(Box<FunctionValue>),
    Reference(u32),
    Tuple(Vec<Self>),
    List(Vec<Self>),
    Map(Vec<(Self, Self)>),
    Set(Vec<Self>),
    Record(Vec<(String, Self)>),
    Error {
        kind: String,
        message: String,
        data: Box<Self>,
    },
}

impl CaptureValue {
    pub(crate) fn children(&self) -> Vec<&Self> {
        match self {
            Self::Tuple(items) | Self::List(items) | Self::Set(items) => items.iter().collect(),
            Self::Map(entries) => entries
                .iter()
                .flat_map(|(key, value)| [key, value])
                .collect(),
            Self::Record(fields) => fields.iter().map(|(_, value)| value).collect(),
            Self::Error { data, .. } => vec![data],
            _ => Vec::new(),
        }
    }
}

impl FunctionValue {
    /// Nodes in canonical first-visit order, with the root at index zero.
    pub fn nodes(&self) -> Vec<&Self> {
        let mut nodes = vec![self];
        let mut pending: Vec<_> = self.captures.values().rev().collect();
        while let Some(value) = pending.pop() {
            if let CaptureValue::Function(function) = value {
                nodes.push(function);
                pending.extend(function.captures.values().rev());
            } else {
                pending.extend(value.children().into_iter().rev());
            }
        }
        nodes
    }

    pub(crate) fn check_graph(&self, depth: usize) -> Result<(), FunctionValueRefusal> {
        let mut count = 1u32;
        self.check_token(depth)?;
        let mut pending: Vec<_> = self
            .captures
            .values()
            .rev()
            .map(|value| (value, depth))
            .collect();
        while let Some((value, remaining)) = pending.pop() {
            let remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| invalid("callable captures exceed the value depth bound"))?;
            match value {
                CaptureValue::Reference(index) if *index >= count => {
                    return Err(invalid(
                        "callable reference must name an earlier environment node",
                    ));
                }
                CaptureValue::Function(function) => {
                    count += 1;
                    if count > Self::MAX_ENVIRONMENT_NODES {
                        return Err(invalid("callable environment has too many nodes"));
                    }
                    function.check_token(remaining)?;
                    pending.extend(
                        function
                            .captures
                            .values()
                            .rev()
                            .map(|value| (value, remaining)),
                    );
                }
                CaptureValue::Data(datum) => check_data(datum, remaining)?,
                _ => pending.extend(
                    value
                        .children()
                        .into_iter()
                        .rev()
                        .map(|value| (value, remaining)),
                ),
            }
        }
        Ok(())
    }

    fn check_token(&self, depth: usize) -> Result<(), FunctionValueRefusal> {
        match (&self.representation, &self.token) {
            (FunctionRepresentation::Bare, None) => Ok(()),
            (FunctionRepresentation::Token { tag, callable }, Some(values))
                if *callable > 0
                    && values.first() == Some(&Datum::Text(tag.clone()))
                    && values.get(*callable as usize) == Some(&Datum::Null) =>
            {
                for value in values {
                    check_data(value, depth)?;
                }
                Ok(())
            }
            _ => Err(invalid("callable token differs from its representation")),
        }
    }

    /// A graph is bounded independently of its back references and byte bound.
    pub const MAX_ENVIRONMENT_NODES: u32 = 1024;
}

fn invalid(problem: &str) -> FunctionValueRefusal {
    FunctionValueRefusal::Invalid {
        problem: problem.into(),
    }
}
fn check_data(datum: &Datum, depth: usize) -> Result<(), FunctionValueRefusal> {
    let mut pending = vec![(datum, depth)];
    while let Some((datum, remaining)) = pending.pop() {
        let remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| invalid("callable data exceeds the value depth bound"))?;
        match datum {
            Datum::Function(_) | Datum::FunctionValue(_) | Datum::Handle(_) => {
                return Err(invalid(
                    "callable data contains an unbound function or live handle",
                ));
            }
            Datum::Tuple(values) | Datum::List(values) | Datum::Set(values) => {
                pending.extend(values.iter().map(|value| (value, remaining)))
            }
            Datum::Record(values) => {
                pending.extend(values.iter().map(|(_, value)| (value, remaining)))
            }
            Datum::Map(values) => pending.extend(
                values
                    .iter()
                    .flat_map(|(key, value)| [(key, remaining), (value, remaining)]),
            ),
            Datum::Error(error) => pending.push((&error.data, remaining)),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Captions, Document, FunctionRegistry, FunctionValueLimits, NumberPolicy};
    use lash_vocabulary::data_limits::KernelDataLimits;
    use lash_vocabulary::{CallableId, Signature, Type, names::Name};
    use std::collections::BTreeMap;

    fn function() -> FunctionValue {
        FunctionValue {
            signature_origin: None,
            implicit_text_conversions: Vec::new(),
            callable: CallableId {
                link: "11".repeat(32).parse().unwrap(),
                function: lash_vocabulary::FunctionId::parse(&"22".repeat(32)).unwrap(),
            },
            captures: BTreeMap::new(),
            signature: Signature {
                params: vec![],
                result: Type::Int,
            },
            convention: crate::StartConvention::Direct,
            captions: Captions::empty(Document::new(NumberPolicy::Float, vec![]).identity()),
            representation: FunctionRepresentation::Bare,
            creation: None,
            token: None,
        }
    }
    /// K-CLO-004 graph nodes are visited once, canonical, and may refer only backward.
    #[test]
    fn a_recursive_callable_graph_is_canonical_bounded_and_only_refers_backward() {
        let mut root = function();
        let mut child = function();
        child
            .captures
            .insert(Name::new("root"), CaptureValue::Reference(0));
        child
            .captures
            .insert(Name::new("self"), CaptureValue::Reference(1));
        root.captures
            .insert(Name::new("self"), CaptureValue::Reference(0));
        root.captures
            .insert(Name::new("child"), CaptureValue::Function(Box::new(child)));
        let catalog = FunctionRegistry::default();
        let data = KernelDataLimits::standard();
        let limits = FunctionValueLimits::standard();
        root.check(&catalog, limits, data).unwrap();
        assert_eq!(root.nodes().len(), 2);
        let bytes = root.canonical_tagged_bytes().unwrap();
        let roundtrip = FunctionValue::from_transport_json(
            &root.transport_json().unwrap(),
            &catalog,
            limits,
            data,
        )
        .unwrap();
        assert_eq!(roundtrip.canonical_tagged_bytes().unwrap(), bytes);
        let mut reordered = root.clone();
        let fields = std::mem::take(&mut reordered.captures);
        for (name, value) in fields.into_iter().rev() {
            reordered.captures.insert(name, value);
        }
        assert_eq!(reordered.canonical_tagged_bytes().unwrap(), bytes);
        let narrow = FunctionValueLimits::new(bytes.len() as u64 - 1).unwrap();
        assert!(matches!(
            root.check(&catalog, narrow, data),
            Err(FunctionValueRefusal::TooLarge { .. })
        ));
        let mut invalid_graph = root.clone();
        invalid_graph
            .captures
            .insert(Name::new("ahead"), CaptureValue::Reference(1));
        assert!(matches!(
            invalid_graph.check(&catalog, limits, data),
            Err(FunctionValueRefusal::Invalid { .. })
        ));
    }
}
