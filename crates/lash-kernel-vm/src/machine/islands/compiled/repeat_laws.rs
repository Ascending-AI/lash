//! Test access to actual persisted occurrences and source loop controls.
use crate::machine::{Control, KernelMachine};
use lash_kernel_state::paged::OccurrenceRecord;

pub(crate) fn seed_actions(machine: &mut KernelMachine, count: u64) -> usize {
    let exe = &machine.exe;
    let access = machine.heap.pages().clone();
    let task = machine.tasks.get_mut(0).unwrap().unwrap();
    let mut seeded = 0;
    for index in 0..exe.occurrence_count() {
        // Only caller actions lexically within a loop, not the entry call
        // or a helper body's independent invocation-local coordinates.
        let Some(site) = exe.occurrence_site(index) else {
            continue;
        };
        let in_loop = exe.blocks().flat_map(|block| block.stmts.iter()).any(|id| {
            let stmt = exe.stmt(*id);
            matches!(stmt.form, crate::compile::Form::While { .. })
                && stmt.site.unit == site.unit
                && site.path.starts_with(&stmt.site.path)
        });
        if in_loop {
            task.occurrences
                .write(
                    exe.occurrence_key(index).to_vec(),
                    OccurrenceRecord {
                        owner: exe.occurrence_owner(index),
                        count,
                    },
                    &access,
                )
                .unwrap();
            seeded += 1;
        }
    }
    task.published = None;
    // Read genuine working counters from the saved rows without changing
    // the original activation's entry/resume state.
    let mut record = task.record.clone();
    record.frames = task.frames.rows.clone();
    let mut rows = lash_kernel_state::kernel_internals::tree::Tree::new(false);
    rows.write(
        lash_kernel_state::kernel_internals::directory::ordinal_key(0),
        record,
        &access,
    )
    .unwrap();
    let mut reopened = crate::machine::tasks::Tasks::open(
        machine.logical.clone(),
        rows,
        access,
        machine.arena.clone(),
        machine.exe.clone(),
    );
    let counters = std::mem::replace(
        &mut reopened.get_mut(0).unwrap().unwrap().counters,
        crate::machine::tasks::Occurrences::for_source_law(machine.exe.occurrence_count()),
    );
    machine.tasks.get_mut(0).unwrap().unwrap().counters = counters;
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
        .ledger()
        .borrow()
        .prospective_invocation(lash_kernel_doc::TaskId(0))
        .id()
}
