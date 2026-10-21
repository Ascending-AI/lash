//! Canonical token encodings of source coordinates and document correspondence.

use crate::Correspondence;
use crate::SourceLanguage;
use crate::SourceMap;
use crate::SourceSite;
use crate::Span;
use crate::Survivor;
use lash_vocabulary::names::DocumentId;
use lash_vocabulary::sites::Site;
use lash_vocabulary::tokens::CodecVariant;
use lash_vocabulary::tokens::TokenCodec;
use lash_vocabulary::tokens::TokenError;
use lash_vocabulary::tokens::TokenReader;
use lash_vocabulary::tokens::TokenWriter;

impl TokenCodec for Span {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.unsigned(self.start as u64);
        out.unsigned(self.end as u64);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Self::new(
            lash_vocabulary::tokens::checked_usize(input.unsigned()?)?,
            lash_vocabulary::tokens::checked_usize(input.unsigned()?)?,
        )
        .ok_or_else(|| TokenError::Invalid("source range ends before it starts".into()))
    }
}
impl TokenCodec for SourceLanguage {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 0);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 0)?;
        match variant {
            0 => Ok(Self::TypeScript),
            1 => Ok(Self::Python),
            _ => Err(TokenError::UnsupportedVariant { variant }),
        }
    }
}
impl TokenCodec for SourceSite {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.site.encode(out);
        self.ranges.encode(out);
        self.implicit.map(|offset| offset as u64).encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            site: Site::decode(input)?,
            ranges: Vec::decode(input)?,
            implicit: Option::<u64>::decode(input)?
                .map(lash_vocabulary::tokens::checked_usize)
                .transpose()?,
        })
    }
}
impl TokenCodec for SourceMap {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        out.bytes(&self.source);
        self.document.encode(out);
        self.language.encode(out);
        self.sites.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        let source = input
            .bytes()?
            .try_into()
            .map_err(|_| TokenError::Invalid("source digest must be 32 bytes".into()))?;
        let map = Self {
            source,
            document: DocumentId::decode(input)?,
            language: SourceLanguage::decode(input)?,
            sites: Vec::decode(input)?,
        };
        map.check_order()
            .map_err(|error| TokenError::Invalid(error.to_string()))?;
        Ok(map)
    }
}
impl TokenCodec for Survivor {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.from.encode(out);
        self.to.encode(out);
        self.edited.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            from: Site::decode(input)?,
            to: Site::decode(input)?,
            edited: bool::decode(input)?,
        })
    }
}
impl TokenCodec for Correspondence {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.base.encode(out);
        self.result.encode(out);
        out.seq(self.entries().len() as u64);
        for entry in self.entries() {
            entry.encode(out);
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        let base = DocumentId::decode(input)?;
        let result = DocumentId::decode(input)?;
        let entries = Vec::<Survivor>::decode(input)?;
        if entries.windows(2).any(|pair| pair[0].from >= pair[1].from) {
            return Err(TokenError::Invalid(
                "correspondence entries are unsorted or repeated".into(),
            ));
        }
        Self::of(base, result, entries)
            .ok_or_else(|| TokenError::Invalid("correspondence repeats a destination site".into()))
    }
}
