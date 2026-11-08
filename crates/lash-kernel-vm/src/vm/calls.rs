//! The native-call ABI (cutover contract §1, §2): a native reads its
//! arguments through zero-copy [`Args`] views over the heap's bodies and
//! builds into a [`Nursery`] the machine installs after it returns.
//!
//! A call that asks for room is retried once after a collection, then
//! refused at the memory bound. A Decline runs the definition's body
//! (`K-LIB-006`); it creates no step (R3).

use lash_kernel_doc::Value;
use lash_kernel_doc::WorkCounter;
use lash_kernel_doc::arena::native::{Args, NativeCall, NativeError, Nursery, Retry};
use lash_kernel_doc::heap::LogicalBytes;

use super::{Flow, Vm};
use crate::bytecode::program::{NativeEntry, Program};
use crate::heap::{Heap, MemoryRefused};

/// How a native call ended, when it did not fail the run.
pub(crate) enum Answer {
    Value(Value),
    /// The native declined: the body runs the call.
    Declined,
}

impl<H: Heap> Vm<H> {
    /// Calls native `native` of `program` with its top `argc` operands,
    /// popping them unless it declines.
    pub(crate) fn call_native(&mut self, program: &Program, native: u32) -> Result<Answer, Flow> {
        let entry: &NativeEntry = &program.natives[native as usize];
        let argc = usize::from(entry.argc);
        let from = self.stack.len() - argc;
        let mut collected = false;
        loop {
            let room = LogicalBytes(self.room());
            let objects = self.heap.objects();
            let semantics = self.heap.semantics();
            let args = Args::new(&self.stack[from..], objects, &semantics);
            let mut nursery = Nursery::new(objects, room);
            let mut counter = WorkCounter::new(None);
            let answer = entry.native.call(NativeCall {
                args,
                new: &mut nursery,
                counter: &mut counter,
            });
            match answer {
                Ok(value) => {
                    let held = nursery.held().0;
                    let built = nursery.into_new();
                    self.heap.admit(held).map_err(Flow::Memory)?;
                    self.heap.install(built).map_err(Flow::native)?;
                    self.stack.truncate(from);
                    return Ok(Answer::Value(value));
                }
                Err(NativeError::Retry(Retry::Room { wanted })) => {
                    nursery.discard();
                    if collected {
                        return Err(Flow::Memory(MemoryRefused {
                            held: self.heap.held(),
                            wanted: wanted.0,
                        }));
                    }
                    self.collect();
                    collected = true;
                }
                Err(NativeError::Decline) => {
                    nursery.discard();
                    return Ok(Answer::Declined);
                }
                Err(error) => {
                    nursery.discard();
                    self.stack.truncate(from);
                    return Err(Flow::native(error));
                }
            }
        }
    }
}
