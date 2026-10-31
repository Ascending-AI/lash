//! Source positions distinguish an active loop body from its next header.
use super::*;
use crate::compile::{Executable, Form};
use crate::machine::{Control, Frame};
use lash_kernel_value::sites::Site;

pub(super) fn source_loops(
    frame: &Frame,
    exe: &Executable,
) -> Result<Option<Vec<(Site, u64)>>, HeapFault> {
    if frame.cleaning.is_some() {
        return Ok(None);
    }
    let mut loops = Vec::new();
    for (index, control) in frame.control.iter().enumerate() {
        let (stmt, iteration) = match control {
            Control::For { stmt, started, .. } => (*stmt, started.saturating_sub(1)),
            // A condition runs with the While itself on top. Its next body
            // has not been entered yet; nested body controls name started - 1.
            Control::While { stmt, started } => (
                *stmt,
                started.saturating_sub(u64::from(index + 1 < frame.control.len())),
            ),
            Control::Try { .. } => return Ok(None),
            _ => continue,
        };
        loops.try_reserve(1).map_err(|_| HeapFault::Exhausted {
            resource: doc::heap::HostResource::StagingBytes,
        })?;
        let (Form::For { site, .. } | Form::While { site, .. }) = &exe.stmt(stmt).form else {
            return Err(malformed("source loop stands at another form"));
        };
        loops.push((site.clone(), iteration));
    }
    Ok(Some(loops))
}
