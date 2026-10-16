//! A function crossing to a tool as data (`K-EFF-002`, `K-CLO-004`).
//!
//! A function a run hands to a tool leaves it as a [`FunctionValue`]: a
//! small document of its own holding the function's code, everything that
//! code reaches, and its captures frozen as constants, with one entry that
//! starts it. Nothing is stored at the crossing; the receiver decides
//! whether to start, store or ignore the value. The [`StartAdapter`] a
//! closure carries says how the entry calls it: the kernel builds the
//! entry from the adapter, so no dialect runs at the crossing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::document::Document;
use crate::name::{FunctionId, Name};
use crate::types::Signature;

/// The record key a function value's transport wraps it under:
/// `{"$lash_function": <FunctionValue>}`. Only the typed encoder writes it;
/// an ordinary record key in this prefix is escaped (`K-EFF-002`).
pub const FUNCTION_VALUE_TAG: &str = "$lash_function";

/// `value` written as canonical JSON: members sorted by name as UTF-8
/// bytes (serde_json's map is ordered: no `preserve_order`), no white space.
fn canonical_json(value: &serde_json::Value) -> Result<Vec<u8>, crate::EncodeError> {
    serde_json::to_vec(value).map_err(|error| crate::EncodeError {
        message: error.to_string(),
    })
}

/// A guest record key as the transport carries it: a key in the reserved
/// [`FUNCTION_VALUE_TAG`] prefix is escaped by doubling the prefix, so no
/// record a guest writes reads as a function value. Only the typed encoder
/// of a [`crate::Datum::FunctionValue`] writes the tag itself.
#[must_use]
pub fn escape_function_key(key: &str) -> std::borrow::Cow<'_, str> {
    if key.starts_with(FUNCTION_VALUE_TAG) {
        std::borrow::Cow::Owned(format!("{FUNCTION_VALUE_TAG}{key}"))
    } else {
        std::borrow::Cow::Borrowed(key)
    }
}

/// The name of the one entry a function value's document has.
pub const FUNCTION_VALUE_ENTRY: &str = "run";

/// A function as data: a self-contained document and the entry that starts
/// it. The entry's signature is `document.entries[entry]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunctionValue {
    pub document: Document,
    pub entry: Name,
}

/// How a function a closure makes is started from outside its run: the
/// signature it is started under, how the dialect holds the function as a
/// value, and how the entry calls it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartAdapter {
    pub signature: Signature,
    pub representation: FunctionRepresentation,
    pub convention: StartConvention,
}

impl StartAdapter {
    /// The library functions starting through this adapter calls: code
    /// that holds it reaches them (`K-DOC-002`).
    pub fn functions(&self) -> Vec<FunctionId> {
        match &self.convention {
            StartConvention::Direct => Vec::new(),
            StartConvention::TsArgsList { await_fn, same_fn } => vec![*await_fn, *same_fn],
        }
    }
}

/// How a dialect holds a function as a value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum FunctionRepresentation {
    /// The closure itself.
    Bare,
    /// A tuple whose first member is the text `tag` and whose member at
    /// `callable` is the closure. The tuple crosses as one function value.
    Token { tag: String, callable: u32 },
}

impl FunctionRepresentation {
    /// The tag of the token a function value is held in, if it is.
    pub fn tag(&self) -> Option<&str> {
        match self {
            Self::Bare => None,
            Self::Token { tag, .. } => Some(tag),
        }
    }
}

/// How the entry of a function value calls the function.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StartConvention {
    /// With the start's arguments, in the signature's order, returning what
    /// the function returns.
    Direct,
    /// With an absent `this` and the list of the start's arguments; the
    /// returned value is awaited by `await_fn`, and an absent result
    /// (`same_fn` compares it with absent) ends the start with null.
    TsArgsList {
        await_fn: FunctionId,
        same_fn: FunctionId,
    },
}

/// The bound on one function value: its canonical tagged JSON, counting its
/// code and frozen captures. The library functions its manifest lists are
/// declarations a consumer holds by identity, through helper bodies too
/// (`K-DOC-002`), so that listing is not counted
/// ([`FunctionValue::counted_bytes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunctionValueLimits {
    pub max_canonical_bytes: u64,
}

impl FunctionValueLimits {
    /// The hard ceiling: 64 KiB of canonical tagged JSON.
    pub const CEILING: u64 = 64 * 1024;

    pub const fn standard() -> Self {
        Self {
            max_canonical_bytes: Self::CEILING,
        }
    }

    /// A host's narrower bound; `None` above the ceiling.
    pub const fn new(max_canonical_bytes: u64) -> Option<Self> {
        if max_canonical_bytes > Self::CEILING {
            None
        } else {
            Some(Self {
                max_canonical_bytes,
            })
        }
    }
}

impl Default for FunctionValueLimits {
    fn default() -> Self {
        Self::standard()
    }
}

/// Why bytes or JSON are not a function value a consumer may admit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum FunctionValueRefusal {
    /// The JSON is not the one-key `$lash_function` wrapper.
    #[error("a function value is the one-key `{FUNCTION_VALUE_TAG}` record")]
    NotTagged,
    /// Its canonical tagged JSON is larger than the bound.
    #[error("the function value takes {bytes} canonical bytes, above the bound of {limit}")]
    TooLarge { bytes: u64, limit: u64 },
    /// The payload does not decode as a function value.
    #[error("the function value does not decode: {problem}")]
    Decode { problem: String },
    /// Its document is not a valid document.
    #[error("the function value's document is invalid: {problem}")]
    Invalid { problem: String },
    /// Its document has no entry of its entry's name.
    #[error("the function value's document has no entry `{entry}`")]
    NoEntry { entry: Name },
}

impl FunctionValue {
    /// The signature the value is started under.
    pub fn signature(&self) -> Option<&Signature> {
        self.document.entries.get(&self.entry)
    }

    /// The canonical bytes of the transport form, `{"$lash_function": ..}`,
    /// whose length the limits bound.
    ///
    /// # Errors
    ///
    /// A value that cannot be written as JSON.
    pub fn canonical_tagged_bytes(&self) -> Result<Vec<u8>, crate::EncodeError> {
        canonical_json(&self.transport_json()?)
    }

    /// The bytes [`FunctionValueLimits`] bounds: the canonical transport
    /// form with its manifest's library listing left out.
    ///
    /// # Errors
    ///
    /// A value that cannot be written as JSON.
    pub fn counted_bytes(&self) -> Result<u64, crate::EncodeError> {
        let mut counted = self.transport_json()?;
        if let Some(listed) = counted.pointer_mut(&format!(
            "/{FUNCTION_VALUE_TAG}/document/manifest/functions"
        )) {
            *listed = serde_json::Value::Object(serde_json::Map::new());
        }
        Ok(canonical_json(&counted)?.len() as u64)
    }

    /// The transport form, `{"$lash_function": <FunctionValue>}`.
    ///
    /// # Errors
    ///
    /// A value that cannot be written as JSON.
    pub fn transport_json(&self) -> Result<serde_json::Value, crate::EncodeError> {
        let payload = serde_json::to_value(self).map_err(|error| crate::EncodeError {
            message: error.to_string(),
        })?;
        let mut wrapper = serde_json::Map::with_capacity(1);
        wrapper.insert(FUNCTION_VALUE_TAG.to_owned(), payload);
        Ok(serde_json::Value::Object(wrapper))
    }

    /// Checks the value against `limits` and the document rules: its
    /// canonical size, a valid document, and an entry of its entry's name.
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn check(
        &self,
        catalog: &dyn crate::FunctionCatalog,
        limits: FunctionValueLimits,
        data: crate::KernelDataLimits,
    ) -> Result<(), FunctionValueRefusal> {
        let bytes = self
            .counted_bytes()
            .map_err(|error| FunctionValueRefusal::Decode {
                problem: error.to_string(),
            })?;
        if bytes > limits.max_canonical_bytes {
            return Err(FunctionValueRefusal::TooLarge {
                bytes,
                limit: limits.max_canonical_bytes,
            });
        }
        crate::validate_document_with_limits(&self.document, catalog, data).map_err(|error| {
            FunctionValueRefusal::Invalid {
                problem: error.to_string(),
            }
        })?;
        if self.signature().is_none() || !self.document.functions.contains_key(&self.entry) {
            return Err(FunctionValueRefusal::NoEntry {
                entry: self.entry.clone(),
            });
        }
        Ok(())
    }

    /// Reads a tool input's `{"$lash_function": ..}` wrapper, its document
    /// decoded within `data`, and checks it against `limits` and `catalog`
    /// ([`Self::check`]). The
    /// tag carries a value, not authority: a consumer still admits the
    /// document against its own launch environment.
    ///
    /// # Errors
    ///
    /// Why the JSON is no admissible function value.
    pub fn from_transport_json(
        json: &serde_json::Value,
        catalog: &dyn crate::FunctionCatalog,
        limits: FunctionValueLimits,
        data: crate::KernelDataLimits,
    ) -> Result<Self, FunctionValueRefusal> {
        let serde_json::Value::Object(wrapper) = json else {
            return Err(FunctionValueRefusal::NotTagged);
        };
        let (Some(payload), 1) = (wrapper.get(FUNCTION_VALUE_TAG), wrapper.len()) else {
            return Err(FunctionValueRefusal::NotTagged);
        };
        let value = Self::from_payload(payload, data)?;
        value.check(catalog, limits, data)?;
        Ok(value)
    }

    /// The `{document, entry}` payload, its document read under the
    /// document's own version and depth rules.
    fn from_payload(
        payload: &serde_json::Value,
        data: crate::KernelDataLimits,
    ) -> Result<Self, FunctionValueRefusal> {
        let decode = |problem: String| FunctionValueRefusal::Decode { problem };
        let serde_json::Value::Object(fields) = payload else {
            return Err(decode("the payload is not a record".to_owned()));
        };
        let (Some(document), Some(serde_json::Value::String(entry)), 2) =
            (fields.get("document"), fields.get("entry"), fields.len())
        else {
            return Err(decode(
                "the payload is the record of `document` and `entry`".to_owned(),
            ));
        };
        let document = Document::from_json_with_limits(&document.to_string(), data)
            .map_err(|error| decode(error.to_string()))?;
        Ok(Self {
            document,
            entry: Name::new(entry.as_str()),
        })
    }
}

/// Carries the start adapters of `from` into `into`, which a reader built
/// from source printed from `from`: source spells no adapter, and the
/// document it is read back against keeps them. A closure takes the adapter
/// of the closure in the same place of the same unit when both take the
/// same parameters; a declared function takes its namesake's.
pub fn carry_start_adapters(from: &Document, into: &mut Document) {
    let starts = |document: &mut Document| {
        let mut units = Vec::new();
        for block in std::iter::once(&mut document.main).chain(
            document
                .functions
                .values_mut()
                .map(|function| &mut function.body),
        ) {
            let mut found = Vec::new();
            crate::ast::each_closure_mut(block, &mut |closure| {
                found.push((closure.params.clone(), closure.start.clone()));
            });
            units.push(found);
        }
        units
    };
    let mut source = from.clone();
    let carried = starts(&mut source);
    let same_units = from.functions.keys().eq(into.functions.keys());
    for (name, function) in &mut into.functions {
        if let Some(found) = from.functions.get(name)
            && found.params == function.params
        {
            function.start.clone_from(&found.start);
        }
    }
    if !same_units {
        return;
    }
    for (block, found) in std::iter::once(&mut into.main)
        .chain(
            into.functions
                .values_mut()
                .map(|function| &mut function.body),
        )
        .zip(carried)
    {
        let mut at = 0;
        crate::ast::each_closure_mut(block, &mut |closure| {
            if let Some((params, start)) = found.get(at)
                && *params == closure.params
            {
                closure.start.clone_from(start);
            }
            at += 1;
        });
    }
}
