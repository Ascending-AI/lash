//! The caption conformance harness (`K-CAP-001`..`K-CAP-006`), the same for
//! every dialect: it takes a dialect [`Package`] and that dialect's
//! [`CaptionCorpus`], and checks each rule against what the package's front
//! end, printer and reader do. A dialect that spells no caption declares no
//! [`lash_kernel_dialect::CaptionSupport::kinds`]: every caption it prints is
//! dropped, and nothing else is checked of it.
//!
//! A dialect gains captions by declaring the kinds it spells and adding
//! authored cases to its corpus; the laws stay as they are.

use std::collections::BTreeSet;

use lash_kernel_dialect::{
    Environment, Lowered, Mode, Package, PrintError, PrintOptions, SourceFormat,
};
use lash_kernel_doc::{Caption, CaptionEntry, CaptionKind, Captions, Document, Site};
use serde::Deserialize;

use crate::HarnessError;

/// One dialect's caption cases, in its own spelling.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptionCorpus {
    /// Workflow modules whose captions are spelled in the source
    /// (`K-CAP-001`, `K-CAP-003`).
    #[serde(default)]
    pub authored: Vec<AuthoredCase>,
    /// Workflow modules whose documents are printed with a caption on every
    /// captionable site and read back (`K-CAP-002`, `K-CAP-005`, `K-CAP-006`).
    #[serde(default)]
    pub readback: Vec<ReadbackCase>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredCase {
    pub name: String,
    pub source: String,
    /// `source` with its caption syntax removed.
    pub stripped: String,
    /// Exactly what importing `source` answers.
    pub captions: Vec<CaptionEntry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadbackCase {
    pub name: String,
    pub source: String,
}

/// A dialect under test: its package, what its modules lower against, the
/// generated format its printer writes, and its corpus.
pub struct CaptionDialect<'a> {
    pub package: &'a Package,
    pub environment: Environment<'a>,
    pub format: SourceFormat,
    pub corpus: &'a CaptionCorpus,
}

impl CaptionCorpus {
    /// A corpus file.
    ///
    /// # Errors
    ///
    /// A file that is not a corpus.
    pub fn from_json(text: &str) -> Result<Self, HarnessError> {
        serde_json::from_str(text).map_err(|error| HarnessError(format!("caption corpus: {error}")))
    }
}

fn fail(rule: &str, case: &str, message: impl std::fmt::Display) -> HarnessError {
    HarnessError(format!("{rule} {case}: {message}"))
}

impl CaptionDialect<'_> {
    fn module(&self, rule: &str, case: &str, source: &str) -> Result<Lowered, HarnessError> {
        self.package
            .front_end
            .lower(source, &self.environment, Mode::Module)
            .map_err(|diagnostic| fail(rule, case, format!("{diagnostic:?}\n{source}")))
    }

    fn print(
        &self,
        document: &Document,
        captions: &Captions,
        source_map: bool,
    ) -> Result<lash_kernel_dialect::PrintedSource, PrintError> {
        let printer = self
            .package
            .printer
            .as_ref()
            .ok_or_else(|| PrintError::Invalid("the dialect has no printer".into()))?;
        printer.print(
            document,
            Some(captions),
            PrintOptions {
                source_map,
                ..Default::default()
            },
        )
    }

    /// `K-CAP-001`: a source and the same source without its caption syntax
    /// lower to one document with one source-map site set.
    ///
    /// # Errors
    ///
    /// The first case that differs.
    pub fn behaviour_free(&self) -> Result<(), HarnessError> {
        const RULE: &str = "K-CAP-001";
        for case in &self.corpus.authored {
            let captioned = self.module(RULE, &case.name, &case.source)?;
            let stripped = self.module(RULE, &case.name, &case.stripped)?;
            if captioned.document.id() != stripped.document.id() {
                return Err(fail(
                    RULE,
                    &case.name,
                    "caption syntax changed the document",
                ));
            }
            let sites = |lowered: &Lowered| -> BTreeSet<Site> {
                lowered
                    .source_map
                    .iter()
                    .flat_map(|map| map.sites.iter().map(|located| located.site.clone()))
                    .collect()
            };
            if sites(&captioned) != sites(&stripped) {
                return Err(fail(
                    RULE,
                    &case.name,
                    "caption syntax changed the map's sites",
                ));
            }
            if !stripped.captions.is_empty() {
                return Err(fail(RULE, &case.name, "the stripped source has captions"));
            }
        }
        Ok(())
    }

    /// `K-CAP-003`: an authored source imports with exactly its captions,
    /// each on the site that owns it.
    ///
    /// # Errors
    ///
    /// The first case whose table differs.
    pub fn authored(&self) -> Result<(), HarnessError> {
        const RULE: &str = "K-CAP-003";
        for case in &self.corpus.authored {
            let lowered = self.module(RULE, &case.name, &case.source)?;
            let expected = Captions::new_identified(&lowered.document, case.captions.clone())
                .map_err(|refusal| fail(RULE, &case.name, refusal))?;
            if lowered.captions != expected {
                return Err(fail(
                    RULE,
                    &case.name,
                    format!(
                        "imported {:?}\nexpected {:?}",
                        lowered.captions.entries(),
                        expected.entries()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// `K-CAP-002`: printing a document with a caption on every captionable
    /// site, without a source map, and reading the source back against it
    /// answers the document and exactly the captions of the kinds the
    /// dialect spells.
    ///
    /// # Errors
    ///
    /// The first case that does not read back.
    pub fn readback(&self) -> Result<(), HarnessError> {
        const RULE: &str = "K-CAP-002";
        for case in &self.corpus.readback {
            let lowered = self.module(RULE, &case.name, &case.source)?;
            let document = lowered.document.document();
            let captions = everywhere(document)?;
            let printed = self
                .print(document, &captions, false)
                .map_err(|error| fail(RULE, &case.name, error))?;
            let (read, read_captions) =
                (self.package.read)(&printed.source, document, Default::default())
                    .map_err(|diagnostic| fail(RULE, &case.name, format!("{diagnostic:?}")))?;
            if read.canonical_bytes() != document.canonical_bytes() {
                return Err(fail(
                    RULE,
                    &case.name,
                    format!("the document did not read back\n{}", printed.source),
                ));
            }
            let expected = self.spelled(document, &captions)?;
            if read_captions != expected {
                return Err(fail(
                    RULE,
                    &case.name,
                    format!(
                        "read {:?}\nexpected {:?}\n{}",
                        read_captions.entries(),
                        expected.entries(),
                        printed.source
                    ),
                ));
            }
        }
        Ok(())
    }

    /// `K-CAP-006`: a printer lists in `dropped` exactly the captions of
    /// the kinds its dialect does not spell, with or without a map.
    ///
    /// # Errors
    ///
    /// The first case whose drops differ.
    pub fn dropped(&self) -> Result<(), HarnessError> {
        const RULE: &str = "K-CAP-006";
        let mut cases = 0;
        for case in &self.corpus.readback {
            let lowered = self.module(RULE, &case.name, &case.source)?;
            let document = lowered.document.document();
            let captions = everywhere(document)?;
            let expected: Vec<Site> = captions
                .entries()
                .iter()
                .filter(|entry| !self.supports(document, &entry.site))
                .map(|entry| entry.site.clone())
                .collect();
            for source_map in [false, true] {
                let printed = self
                    .print(document, &captions, source_map)
                    .map_err(|error| fail(RULE, &case.name, error))?;
                if printed.dropped != expected {
                    return Err(fail(
                        RULE,
                        &case.name,
                        format!("dropped {:?}\nexpected {expected:?}", printed.dropped),
                    ));
                }
                if printed.source_map.is_some() != source_map {
                    return Err(fail(RULE, &case.name, "the map was not as requested"));
                }
            }
            cases += 1;
        }
        if cases == 0 {
            return Err(fail(RULE, "corpus", "no readback case"));
        }
        Ok(())
    }

    /// `K-CAP-005`, at the printer: captions that do not validate against the
    /// document printed, decoded rather than built, are refused before any
    /// source is written.
    ///
    /// # Errors
    ///
    /// A printer that accepts them.
    pub fn printer_validates(&self) -> Result<(), HarnessError> {
        const RULE: &str = "K-CAP-005";
        let case = self
            .corpus
            .readback
            .first()
            .ok_or_else(|| fail(RULE, "corpus", "no readback case"))?;
        let lowered = self.module(RULE, &case.name, &case.source)?;
        let document = lowered.document.document();
        let other = lash_kernel_doc::Document::new(document.manifest.numbers, Vec::new());
        let caption = serde_json::json!({"title": "Send the report"});
        let wrong = [
            (
                "another document",
                other.identity(),
                Site::new(lash_kernel_doc::Unit::Main, [0]),
            ),
            (
                "main's body",
                document.identity(),
                Site::new(lash_kernel_doc::Unit::Main, Vec::new()),
            ),
        ];
        for (name, identity, site) in wrong {
            let captions: Captions = serde_json::from_value(serde_json::json!({
                "document": identity,
                "entries": [{"site": site, "caption": caption}],
            }))
            .map_err(|error| fail(RULE, name, error))?;
            match self.print(document, &captions, false) {
                Err(PrintError::Invalid(_)) => {}
                other => return Err(fail(RULE, name, format!("printed {other:?}"))),
            }
        }
        Ok(())
    }

    fn supports(&self, document: &Document, site: &Site) -> bool {
        CaptionKind::of(document, site).is_some_and(|kind| self.package.captions.supports(kind))
    }

    /// `captions` restricted to the kinds the dialect spells.
    fn spelled(&self, document: &Document, captions: &Captions) -> Result<Captions, HarnessError> {
        let kept = captions
            .entries()
            .iter()
            .filter(|entry| self.supports(document, &entry.site))
            .cloned()
            .collect();
        Captions::new(document, kept).map_err(|refusal| HarnessError(refusal.to_string()))
    }
}

/// Texts at the text rule's boundaries, which every dialect must spell
/// exactly: non-ASCII, multi-line with one blank line, the longest title and
/// description, and characters that only start a comment or a tag elsewhere.
///
/// # Errors
///
/// A boundary the text rule refuses.
pub fn boundary_captions() -> Result<Vec<Caption>, HarnessError> {
    let longest_title = "é".repeat(lash_kernel_doc::CAPTION_TITLE_BYTES / 2);
    // 32 lines of 63 bytes, 31 separators and one byte more: the limit.
    let line = "d".repeat(63);
    let mut longest_description = vec![line.as_str(); 32].join("\n");
    longest_description.push('d');
    [
        ("Send the weekly report", None),
        (
            "Prüfe die Größe — 名前を確認",
            Some("Zeile eins\n\nZeile drei: ✓".to_owned()),
        ),
        (longest_title.as_str(), Some(longest_description)),
        ("x", Some("a\nb".to_owned())),
        (
            "Mail ops@example.com * / now",
            Some("50% /* not a comment".to_owned()),
        ),
    ]
    .into_iter()
    .map(|(title, description)| {
        Caption::new(title, description)
            .map_err(|refusal| HarnessError(format!("boundary text: {refusal}")))
    })
    .collect()
}

/// A caption on every captionable site of `document`, cycling through
/// [`boundary_captions`].
///
/// # Errors
///
/// A site [`CaptionKind::captionable`] lists that does not take one.
pub fn everywhere(document: &Document) -> Result<Captions, HarnessError> {
    let texts = boundary_captions()?;
    let entries = CaptionKind::captionable(document)
        .into_iter()
        .zip(texts.iter().cycle())
        .map(|((site, _), caption)| CaptionEntry {
            site,
            caption: caption.clone(),
        })
        .collect();
    Captions::new(document, entries).map_err(|refusal| HarnessError(refusal.to_string()))
}
