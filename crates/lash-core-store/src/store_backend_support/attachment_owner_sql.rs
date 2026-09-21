//! The one place a backend's SQL spells an attachment owner class.
//!
//! `attachment_manifest.owner_kind` carries
//! [`AttachmentOwnerKind`](crate::store::attachment_manifest::AttachmentOwnerKind),
//! whose wire spelling is also the SQL literal the schema's
//! `ck_attachment_manifest_owner_kind` constraint admits. The attachment GC
//! predicates — "this row's owner is a turn a later commit superseded", "this
//! row's owner is a process the registry no longer holds" — each compare that
//! column against one of those labels, and both backends compare it against
//! the same one.
//!
//! These helpers are the `fn(&str) -> String` shape
//! `lash_store_sql::VocabularyTerm` takes, so a neutral statement names the
//! predicate as `{{turn_attachment_owner(manifest.owner_kind)}}` and the
//! renderer expands it once at startup from the enum. The label therefore has
//! exactly one source, the way FIG-2815 and FIG-2844 made the process
//! lifecycle labels have one.

use crate::store::attachment_manifest::AttachmentOwnerKind;

/// `<column> = '<kind>'`: the rows whose attachment owner is `kind`.
///
/// `column` is a SQL identifier the caller owns (`owner_kind`,
/// `manifest.owner_kind`); it is never user input.
fn attachment_owner_kind_predicate_sql(kind: AttachmentOwnerKind, column: &str) -> String {
    format!("{column} = '{}'", kind.as_str())
}

/// `<column> = 'turn'`: an attachment intent a turn owns.
pub fn turn_attachment_owner_predicate_sql(column: &str) -> String {
    attachment_owner_kind_predicate_sql(AttachmentOwnerKind::Turn, column)
}

/// `<column> = 'process'`: an attachment intent a process owns.
pub fn process_attachment_owner_predicate_sql(column: &str) -> String {
    attachment_owner_kind_predicate_sql(AttachmentOwnerKind::Process, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_predicate_spells_the_enums_own_wire_label() {
        assert_eq!(
            turn_attachment_owner_predicate_sql("manifest.owner_kind"),
            format!(
                "manifest.owner_kind = '{}'",
                AttachmentOwnerKind::Turn.as_str()
            )
        );
        assert_eq!(
            process_attachment_owner_predicate_sql("owner_kind"),
            format!("owner_kind = '{}'", AttachmentOwnerKind::Process.as_str())
        );
    }

    /// The schema's CHECK constraint admits exactly these two labels, so a
    /// predicate that spelled anything else would select nothing for ever
    /// rather than fail.
    #[test]
    fn the_predicates_cover_every_owner_class_the_column_can_hold() {
        for (kind, predicate) in [
            (
                AttachmentOwnerKind::Turn,
                turn_attachment_owner_predicate_sql("owner_kind"),
            ),
            (
                AttachmentOwnerKind::Process,
                process_attachment_owner_predicate_sql("owner_kind"),
            ),
        ] {
            assert_eq!(
                AttachmentOwnerKind::from_wire_str(kind.as_str()),
                Some(kind),
                "{predicate} must compare against a label the decoder admits"
            );
        }
    }
}
