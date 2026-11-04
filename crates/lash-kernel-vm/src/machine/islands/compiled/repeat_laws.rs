//! Test access to actual occurrence counters and source loop controls.
use crate::machine::{Control, KernelMachine};

pub(crate) fn seed_actions(machine: &mut KernelMachine, count: u64) -> usize {
    let exe = &machine.exe;
    let mut counts = vec![0; exe.occurrence_count()];
    let mut seeded = 0;
    for (index, current) in counts.iter_mut().enumerate() {
        // Only caller actions lexically within a loop, not the entry call
        // or a helper body's independent invocation-local coordinates.
        let Some(site) = exe.occurrence_site(index) else {
            continue;
        };
        let canonical = exe.occurrence_index(site).unwrap();
        if canonical != index {
            continue;
        }
        let in_loop = exe.blocks().flat_map(|block| block.stmts.iter()).any(|id| {
            let stmt = exe.stmt(*id);
            matches!(stmt.form, crate::compile::Form::While { .. })
                && stmt.site.unit == site.unit
                && site.path.starts_with(&stmt.site.path)
        });
        if in_loop {
            *current = count;
            seeded += 1;
        }
    }
    machine.tasks.get_mut(0).unwrap().unwrap().counters =
        crate::machine::tasks::Occurrences::for_source_law_counts(&counts);
    seeded
}

pub(crate) fn seed_started(machine: &mut KernelMachine, count: u64) -> bool {
    let task = machine.tasks.get_mut(0).unwrap().unwrap();
    let Some(frame) = task.frames.last_mut().unwrap() else {
        return false;
    };
    let mut control = frame.control.as_slice().to_vec();
    let Some(Control::While { started, .. }) = control
        .iter_mut()
        .rev()
        .find(|control| matches!(control, Control::While { .. }))
    else {
        return false;
    };
    *started = count;
    frame.control.replace(control);
    true
}

pub(crate) fn action_counts(machine: &mut KernelMachine) -> Vec<u64> {
    let len = machine.exe.occurrence_count();
    let task = machine.tasks.get_mut(0).unwrap().unwrap();
    (0..len)
        .map(|at| task.counters.current(at).unwrap())
        .collect()
}

pub(crate) fn invocation_cursor(machine: &KernelMachine) -> u64 {
    machine
        .logical
        .clone()
        .borrow()
        .prospective_invocation(lash_kernel_doc::TaskId(0))
        .id()
}
