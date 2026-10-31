//! Source positions use the current frame's actual entered loop iterations.
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
    for control in &frame.control {
        let (stmt, started) = match control {
            Control::For { stmt, started, .. } | Control::While { stmt, started } => {
                (*stmt, *started)
            }
            Control::Try { .. } => return Ok(None),
            _ => continue,
        };
        loops.try_reserve(1).map_err(|_| HeapFault::Exhausted {
            resource: doc::heap::HostResource::StagingBytes,
        })?;
        let (Form::For { site, .. } | Form::While { site, .. }) = &exe.stmt(stmt).form else {
            return Err(malformed("source loop stands at another form"));
        };
        loops.push((site.clone(), started.saturating_sub(1)));
    }
    Ok(Some(loops))
}
