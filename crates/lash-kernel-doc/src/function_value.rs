//! A function crossing to a tool as data (`K-EFF-002`, `K-CLO-004`).
//!
//! A function a run hands to a tool leaves it as a [`FunctionValue`]: a
//! small document of its own holding the function's code, everything that
//! code reaches, and its captures frozen as constants, with one entry that
//! starts it. Nothing is stored at the crossing; the receiver decides
//! whether to start, store or ignore the value. The [`CallablePresentation`] a
//! closure carries says how the entry calls it: the kernel builds the
//! entry from the adapter, so no dialect runs at the crossing.

use lash_vocabulary::ImportSlot;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use lash_vocabulary::names::FunctionId;
use lash_vocabulary::names::Name;
use lash_vocabulary::types::Signature;

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

/// A callable and frozen captures, retaining the defining Link.
/// The receiving host must validate retained execution provenance before admission.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunctionValue {
    pub callable: lash_vocabulary::names::CallableId,
    pub captures: std::collections::BTreeMap<Name, crate::CaptureValue>,
    pub signature: Signature,
    pub convention: StartConvention,
    pub captions: crate::Captions,
    pub representation: FunctionRepresentation,
    pub creation: Option<lash_vocabulary::sites::Site>,
    /// Token members other than its callable slot, which is Null.
    pub token: Option<Vec<crate::Datum>>,
}

/// How a function a closure makes is started from outside its run: the
/// signature it is started under, how the dialect holds the function as a
/// value, and how the entry calls it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallablePresentation {
    pub signature: Signature,
    pub representation: FunctionRepresentation,
    pub convention: StartConvention,
}

impl CallablePresentation {
    /// The library functions starting through this adapter calls: code
    /// that holds it reaches them (`K-DOC-002`).
    pub fn functions(&self) -> Vec<&FunctionId> {
        self.convention.functions()
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

// function_value: each helper is local to this Document or an explicit Import.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// A convention helper local to this document or named by an explicit Import slot.
pub enum FunctionTarget {
    Local(FunctionId),
    Import(ImportSlot),
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StartConvention {
    Direct,
    TsDirect {
        await_fn: FunctionTarget,
    },
    TsArgsList {
        await_fn: FunctionTarget,
        same_fn: FunctionTarget,
    },
    PyDirect {
        cleanup: FunctionTarget,
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
    #[error(transparent)]
    SignatureRequired { source: Box<SignatureRequired> },
    #[error("function value metadata differs from its defining callable")]
    Identity,
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
    pub fn signature(&self) -> &Signature {
        &self.signature
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
        Ok(self.canonical_tagged_bytes()?.len() as u64)
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
        data: lash_vocabulary::data_limits::KernelDataLimits,
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
        self.check_graph(data.value_depth())?;
        let _ = catalog;
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
        data: lash_vocabulary::data_limits::KernelDataLimits,
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

    fn from_payload(
        payload: &serde_json::Value,
        _data: lash_vocabulary::data_limits::KernelDataLimits,
    ) -> Result<Self, FunctionValueRefusal> {
        serde_json::from_value(payload.clone()).map_err(|error| FunctionValueRefusal::Decode {
            problem: error.to_string(),
        })
    }
}

pub use lash_vocabulary::{
    FunctionLocation, SignaturePosition, SignatureRequired, require_entry_signature,
};

impl CallablePresentation {
    /// Promote this defining declaration, copying its resolved public signature.
    /// This check never applies to ordinary closure creation or calls.
    pub fn promote_entry(
        &self,
        function: FunctionId,
        location: FunctionLocation,
        asynchronous: bool,
        rest: bool,
    ) -> Result<crate::FunctionExport, Box<SignatureRequired>> {
        require_entry_signature(&self.signature, location)?;
        Ok(crate::FunctionExport {
            function,
            signature: self.signature.clone(),
            convention: self.convention.clone(),
            asynchronous,
            rest,
        })
    }
}

impl FunctionValue {
    /// Verify transported public metadata against the defining callable before
    /// promoting it to an entry. A private dialect ABI never supplies a contract.
    pub fn check_definition(
        &self,
        document: &crate::Document,
    ) -> Result<CallablePresentation, FunctionValueRefusal> {
        let defining = if let Some(site) = &self.creation {
            let closure = crate::snapshot::closure_at(document, site)
                .filter(|closure| closure.function == self.callable.function)
                .ok_or(FunctionValueRefusal::Identity)?;
            match &closure.presentation {
                Some(presentation) => presentation.clone(),
                None => self.direct_presentation(document)?,
            }
        } else if let Some(export) = document.exports.values().find_map(|export| match export {
            crate::Export::Function(export) if export.function == self.callable.function => {
                Some(export)
            }
            _ => None,
        }) {
            CallablePresentation {
                signature: export.signature.clone(),
                convention: export.convention.clone(),
                representation: FunctionRepresentation::Bare,
            }
        } else {
            self.direct_presentation(document)?
        };
        if self.signature != defining.signature || self.convention != defining.convention {
            return Err(FunctionValueRefusal::Identity);
        }
        Ok(defining)
    }

    fn direct_presentation(
        &self,
        document: &crate::Document,
    ) -> Result<CallablePresentation, FunctionValueRefusal> {
        let definition = document
            .functions
            .get(&self.callable.function)
            .ok_or(FunctionValueRefusal::Identity)?;
        if self.convention != StartConvention::Direct
            || self.representation != FunctionRepresentation::Bare
        {
            return Err(FunctionValueRefusal::Identity);
        }
        Ok(CallablePresentation {
            signature: definition.signature.clone(),
            convention: StartConvention::Direct,
            representation: FunctionRepresentation::Bare,
        })
    }

    /// Promote a verified defining signature, refusing missing annotations.
    pub fn entry_export(
        &self,
        document: &crate::Document,
    ) -> Result<crate::FunctionExport, FunctionValueRefusal> {
        self.check_definition(document)?
            .promote_entry(
                self.callable.function,
                FunctionLocation {
                    name: None,
                    site: self.creation.clone(),
                },
                false,
                false,
            )
            .map_err(|source| FunctionValueRefusal::SignatureRequired { source })
    }
}
