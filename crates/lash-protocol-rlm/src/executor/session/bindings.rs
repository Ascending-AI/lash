//! A prompt reads bounded binding facts, never the session's whole values.

use super::*;
use crate::session_globals::SessionGlobalKind;
use lash_kernel_vm::SessionValue;

const PREVIEW_BYTES: usize = 8 * 1024;

#[derive(Clone)]
pub(super) struct DefaultValue {
    pub(super) datum: Datum,
    pub(super) identity: u64,
}

impl CodeModeExecutionState {
    /// The session's bindings, excluding saved functions. A bounded read keeps small data and opaque names intact;
    /// larger values are described without copying their contents. Changed
    /// bindings are read once and cached until a cell invalidates them.
    pub(crate) async fn bound_variable_values(
        &mut self,
        pages: Option<&SessionPages>,
    ) -> Result<Vec<(String, crate::session_globals::SessionGlobalKind)>, SessionError> {
        self.note_pages(pages);
        self.load_names().await.map_err(protocol)?;
        self.load_functions().await.map_err(protocol)?;
        let listed: Vec<Name> = self
            .names()
            .into_iter()
            .filter(|name| !self.saved_function(name))
            .collect();
        // A name the store binds is read: its own value lists before any
        // pending default of the name.
        let unread: Vec<Name> = listed
            .iter()
            .filter(|name| !self.view.values.contains_key(*name) && self.bound(name))
            .cloned()
            .collect();
        if !unread.is_empty()
            && let Some(session) = self.current.session
        {
            let read = self
                .pages
                .read(move |source| {
                    let mut editor = SessionEditor::open(Some(&session), source)?;
                    let mut read = Vec::with_capacity(unread.len());
                    for name in &unread {
                        if let Some(value) = editor.preview(name, PREVIEW_BYTES)? {
                            read.push((name.clone(), kind(&value)));
                        }
                    }
                    Ok(read)
                })
                .await
                .map_err(protocol)?;
            self.view.values.extend(read);
        }
        Ok(listed
            .into_iter()
            .filter_map(|name| {
                let kind = match self.view.values.get(&name) {
                    Some(kind) => kind.clone(),
                    None => match self.defaults.get(&name) {
                        Some(value) => default_kind(&value.datum),
                        None => return None,
                    },
                };
                Some((name.to_string(), kind))
            })
            .collect())
    }

    pub(crate) fn binding_revision(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.current.descriptor.hash(&mut hash);
        for (name, value) in &self.defaults {
            name.hash(&mut hash);
            value.identity.hash(&mut hash);
        }
        hash.finish()
    }
}

fn kind(value: &SessionValue) -> SessionGlobalKind {
    match value {
        SessionValue::Data(datum) => data_kind(datum),
        SessionValue::Opaque => SessionGlobalKind::Opaque,
        SessionValue::Unpreviewed { kind, extent } => SessionGlobalKind::Unpreviewed {
            kind: (*kind).into(),
            extent: *extent,
        },
    }
}

fn data_kind(datum: &Datum) -> SessionGlobalKind {
    match datum {
        Datum::Handle(handle) => SessionGlobalKind::Handle {
            kind: handle.kind.clone(),
            id: handle.id.clone(),
        },
        datum => SessionGlobalKind::Data(crate::cell_value::datum_json(datum)),
    }
}

fn default_kind(datum: &Datum) -> SessionGlobalKind {
    if datum_fits(datum, &mut { PREVIEW_BYTES }, 0) {
        return data_kind(datum);
    }
    let (kind, extent) = match datum {
        Datum::Text(text) => ("text", text.len()),
        Datum::Bytes(bytes) => ("bytes", bytes.as_slice().len()),
        Datum::Tuple(items) => ("tuple", items.len()),
        Datum::List(items) => ("list", items.len()),
        Datum::Set(items) => ("set", items.len()),
        Datum::Map(entries) => ("map", entries.len()),
        Datum::Record(fields) => ("record", fields.len()),
        _ => ("value", 1),
    };
    SessionGlobalKind::Unpreviewed {
        kind: kind.into(),
        extent: extent as u64,
    }
}

fn datum_fits(datum: &Datum, remaining: &mut usize, depth: usize) -> bool {
    fn take(remaining: &mut usize, bytes: usize) -> bool {
        match remaining.checked_sub(bytes) {
            Some(left) => {
                *remaining = left;
                true
            }
            None => false,
        }
    }
    if depth >= 64 || !take(remaining, 64) {
        return false;
    }
    match datum {
        Datum::Text(text) => take(remaining, text.len()),
        Datum::Bytes(bytes) => take(remaining, bytes.as_slice().len()),
        Datum::Int(integer) => take(
            remaining,
            usize::try_from(integer.bits().div_ceil(8)).unwrap_or(usize::MAX),
        ),
        Datum::Timestamp(timestamp) => take(
            remaining,
            usize::try_from(timestamp.nanoseconds.bits().div_ceil(8)).unwrap_or(usize::MAX),
        ),
        Datum::Number(number) => take(remaining, number.as_str().len()),
        Datum::Handle(handle) => take(remaining, handle.kind.len().saturating_add(handle.id.len())),
        Datum::Tuple(items) | Datum::List(items) | Datum::Set(items) => items
            .iter()
            .all(|value| datum_fits(value, remaining, depth + 1)),
        Datum::Map(entries) => entries.iter().all(|(key, value)| {
            datum_fits(key, remaining, depth + 1) && datum_fits(value, remaining, depth + 1)
        }),
        Datum::Record(fields) => fields.iter().all(|(name, value)| {
            take(remaining, name.len()) && datum_fits(value, remaining, depth + 1)
        }),
        Datum::Error(error) => {
            take(
                remaining,
                error.kind.len().saturating_add(error.message.len()),
            ) && datum_fits(&error.data, remaining, depth + 1)
        }
        Datum::FunctionValue(_) => false,
        _ => true,
    }
}
