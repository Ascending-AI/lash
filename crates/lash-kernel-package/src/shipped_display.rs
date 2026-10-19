//! The display facts shipped with each of lash's code-only packages.
//!
//! Definition names stay in their language's spelling. They are not exported
//! members, and never pass through canonical member-name parsing.

use lash_helper_labels::{Comparison, LabelError, read_labels};
use lash_kernel_dialect::PackagePath;
use lash_kernel_doc::FunctionName;

use crate::PackageDisplay;

/// Load the display side files belonging to one built-in package.
///
/// This data is attached after decoding its code image and never enters a
/// definition's identity, the artifact digest or a closure node.
pub fn of(package: &PackagePath) -> Result<PackageDisplay, LabelError> {
    let path = package
        .iter()
        .map(|segment| segment.as_str())
        .collect::<Vec<_>>()
        .join(".");
    let (labels, comparisons, replies) = match path.as_str() {
        "kernel" => (
            include_str!("../labels/kernel.tsv"),
            include_str!("../labels/kernel.comparisons.tsv"),
            "",
        ),
        "typescript" => (
            include_str!("../labels/typescript.tsv"),
            include_str!("../labels/typescript.comparisons.tsv"),
            "",
        ),
        "python" => (
            include_str!("../labels/python.tsv"),
            include_str!("../labels/python.comparisons.tsv"),
            include_str!("../labels/python.replies.tsv"),
        ),
        "regex.ecma" => (include_str!("../labels/regex.tsv"), "", ""),
        "date.ecma" => (include_str!("../labels/date.tsv"), "", ""),
        "url.whatwg" => (include_str!("../labels/url.tsv"), "", ""),
        _ => {
            return Err(LabelError {
                line: 0,
                message: format!("no shipped display for package {path}"),
            });
        }
    };
    let mut definitions = read_labels(labels, |name| FunctionName::new(name).ok())?;
    for (index, row) in comparisons.lines().enumerate() {
        let fail = |message: &str| LabelError {
            line: index + 1,
            message: format!("{path} comparisons: {message}"),
        };
        let (name, kind) = row
            .split_once('\t')
            .ok_or_else(|| fail("expected name and comparison"))?;
        let comparison = match kind {
            "Eq" => Comparison::Eq,
            "Ne" => Comparison::Ne,
            "Lt" => Comparison::Lt,
            "Le" => Comparison::Le,
            "Gt" => Comparison::Gt,
            "Ge" => Comparison::Ge,
            "Not" => Comparison::Not,
            "Contains" => Comparison::Contains,
            "StartsWith" => Comparison::StartsWith,
            "EndsWith" => Comparison::EndsWith,
            _ => return Err(fail("unknown comparison")),
        };
        let name = FunctionName::new(name).map_err(|_| fail("invalid definition name"))?;
        let display = definitions
            .get_mut(&name)
            .ok_or_else(|| fail("comparison has no label"))?;
        if display.comparison.replace(comparison).is_some() {
            return Err(fail("duplicate comparison"));
        }
    }
    for (index, row) in replies.lines().enumerate() {
        let fail = |message: &str| LabelError {
            line: index + 1,
            message: format!("{path} replies: {message}"),
        };
        let name = FunctionName::new(row).map_err(|_| fail("invalid definition name"))?;
        let display = definitions
            .get_mut(&name)
            .ok_or_else(|| fail("reply receiver has no label"))?;
        if display.reply {
            return Err(fail("duplicate reply receiver"));
        }
        display.reply = true;
    }
    Ok(PackageDisplay {
        definitions,
        ..PackageDisplay::default()
    })
}
