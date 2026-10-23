//! What a dialect's source wrote of each function it lowered.
//!
//! A front end records one [`Written`] for every closure site of cell code
//! that can outlive its statement: named functions, anonymous ones and
//! lambdas alike. The record is typed and travels with the document that
//! holds the site ([`crate::Lowered::written`]) and with every function a
//! session saves from it ([`crate::SavedFunction::written`]). A saved
//! declaration's row is keyed by its function body. No source
//! map is needed to read it: a later cell of the same dialect calls it by
//! [`Written::call`], with or without the source the cell came from. The
//! executable document owns the function's start adapter and signature.

use std::collections::BTreeMap;

use lash_kernel_doc::{Expr, Node, SourceLanguage, Stmt};

use lash_vocabulary::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use serde::{Deserialize, Serialize};

/// A function as its dialect's source wrote it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Written {
    /// Retained annotation coordinates and typed resolution evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_origin: Option<lash_vocabulary::SignatureOrigin>,
    /// Implicit conversion evidence retained from source authoring.
    pub implicit_text_conversions: Vec<lash_kernel_doc::ImplicitTextConversion>,
    /// The language the function was written in.
    pub language: SourceLanguage,
    /// Its signature in the dialect's own syntax, for display: Python's
    /// `async (x, k=1)`, TypeScript's `(x: number) => Promise<number>`.
    pub signature: String,
    /// How a later cell of the same dialect calls it, where the dialect
    /// places arguments by its parameters: Python writes one, TypeScript
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<CallShape>,
    /// Statement and arm captions at their immutable defining function sites.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captions: Vec<lash_kernel_doc::CaptionEntry>,
}

/// The parameters a call places arguments by, and whether the call makes a
/// coroutine that must be awaited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallShape {
    pub params: Vec<CallParam>,
    pub is_async: bool,
}

/// One parameter of a [`CallShape`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallParam {
    pub name: Name,
    pub has_default: bool,
}

/// One row of a written table as serde writes it: a site is no map key in
/// JSON, so the table is a list of rows in ascending site order.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    site: Site,
    written: Written,
}

/// Serde for a table of [`Written`] keyed by closure site, as a list of
/// `{site, written}` rows in ascending site order. Decoding refuses rows
/// out of order or repeated rather than normalizing them.
pub mod written_table {
    use super::{BTreeMap, Row, Site, Written};
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

    pub fn serialize<S: Serializer>(
        table: &BTreeMap<Site, Written>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let rows: Vec<Row> = table
            .iter()
            .map(|(site, written)| Row {
                site: site.clone(),
                written: written.clone(),
            })
            .collect();
        rows.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<Site, Written>, D::Error> {
        let rows = Vec::<Row>::deserialize(deserializer)?;
        let mut table = BTreeMap::new();
        for row in rows {
            if table
                .last_key_value()
                .is_some_and(|(last, _): (&Site, _)| *last >= row.site)
            {
                return Err(D::Error::custom(format!(
                    "written table rows are unsorted or repeat at {}",
                    row.site
                )));
            }
            table.insert(row.site, row.written);
        }
        Ok(table)
    }
}

impl TokenCodec for Written {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(6);
        self.language.encode(out);
        out.text(&self.signature);
        self.call.encode(out);
        self.captions.encode(out);
        self.signature_origin.encode(out);
        self.implicit_text_conversions.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(6)?;
        Ok(Self {
            language: SourceLanguage::decode(input)?,
            signature: String::decode(input)?,
            call: Option::decode(input)?,
            captions: Vec::decode(input)?,
            signature_origin: Option::decode(input)?,
            implicit_text_conversions: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for CallShape {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.params.encode(out);
        out.bool(self.is_async);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            params: Vec::decode(input)?,
            is_async: input.bool()?,
        })
    }
}

impl TokenCodec for CallParam {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.name.encode(out);
        out.bool(self.has_default);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            name: Name::decode(input)?,
            has_default: input.bool()?,
        })
    }
}

/// The site of the closure `statement` makes, `site` being the
/// statement's own: the closure its right-hand side is. A front end keys a
/// statement's [`Written`] by it, the site a run's closure object carries.
#[must_use]
pub fn closure_site(statement: &Stmt, site: &Site) -> Option<Site> {
    Node::Stmt(statement)
        .children()
        .into_iter()
        .zip(0u32..)
        .find_map(|(child, index)| {
            matches!(child, Node::Expr(Expr::Closure(_))).then(|| site.child(index))
        })
}

/// Captions carried by the written table, at the running document's sites.
/// Closure bodies and saved declarations name their defining function unit.
#[must_use]
pub fn function_captions(
    document: &lash_kernel_doc::Document,
    written: &BTreeMap<Site, Written>,
) -> Vec<lash_kernel_doc::CaptionEntry> {
    let mut captions = BTreeMap::new();
    for written in written.values() {
        for entry in &written.captions {
            if entry
                .target
                .site()
                .is_some_and(|site| document.node(site).is_some())
            {
                captions.insert(entry.target.clone(), entry.clone());
            }
        }
    }
    captions.into_values().collect()
}
