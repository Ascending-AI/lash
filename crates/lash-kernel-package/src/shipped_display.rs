//! The display facts shipped with each of lash's code-only packages.
//!
//! Definition names stay in their language's spelling. They are not exported
//! members, and never pass through canonical member-name parsing.

use lash_helper_labels::LabelError;

use crate::{Package, PackageDisplay};

/// Load one built-in package's JSON display facts, outside execution identity.
pub fn of(package: &Package) -> Result<PackageDisplay, LabelError> {
    let path = package.path().to_string();
    let labels = match path.as_str() {
        "kernel" => include_str!("../labels/kernel.json"),
        "typescript" => include_str!("../labels/typescript.json"),
        "python" => include_str!("../labels/python.json"),
        "regex.ecma" => include_str!("../labels/regex.json"),
        "date.ecma" => include_str!("../labels/date.json"),
        "url.whatwg" => include_str!("../labels/url.json"),
        _ => return Err(LabelError::at("", &path, "no shipped display for package")),
    };
    crate::labels::read(labels, package.exports(), package.roots())
}
