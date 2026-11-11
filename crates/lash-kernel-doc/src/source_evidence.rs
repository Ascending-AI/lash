//! What typed authoring knew of a function's source that its executable
//! document does not hold: evidence beside the document, outside its identity.
//!
//! Evidence never proves a construction or admits a program. A printer may
//! use it to spell source, and that source must still lower back to the same
//! document; evidence that is missing is not guessed.
use lash_kernel_value::Name;
use lash_kernel_value::types::Type;
use {
    lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenError,
    lash_kernel_value::tokens::TokenReader, lash_kernel_value::tokens::TokenWriter,
};

/// A local binding's declared type (`let cursor: string | null`), keyed by
/// the binding's kernel name. A front end gives no two bindings of one
/// lexical scope chain a name in common, and records no type for a name two
/// bindings of a function and the functions it contains declare differently.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DeclaredLocal {
    pub name: Name,
    pub ty: Type,
}

/// A copied function's source evidence: the implicit conversions its source
/// admitted and the declared types of its locals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceEvidence {
    pub implicit_text_conversions: Vec<crate::ImplicitTextConversion>,
    pub declared_locals: Vec<DeclaredLocal>,
}

impl SourceEvidence {
    /// The type the binding `name` was declared with, if evidence names one.
    #[must_use]
    pub fn declared(&self, name: &Name) -> Option<&Type> {
        self.declared_locals
            .iter()
            .find(|local| local.name == *name)
            .map(|local| &local.ty)
    }
}

impl TokenCodec for DeclaredLocal {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.name.encode(out);
        self.ty.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            name: Name::decode(input)?,
            ty: Type::decode(input)?,
        })
    }
}
