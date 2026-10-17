//! The waits issued since the last park (FIG-6425). Wait identities are
//! allocated in order and a park reports every issued wait, so the issued
//! ones are the newest rows of the waits table: a park reads only them,
//! never the waits it reported before.
use super::*;
use lash_kernel_state::kernel_internals::directory::ordinal_key;

/// Where the issued waits start, and how many remain: every wait at or
/// after `from` is issued, and none before it is. Never saved: a machine
/// opened from a checkpoint finds it again from the newest rows.
#[derive(Clone, Copy, Debug)]
pub(super) struct Issued {
    from: u64,
    count: u64,
}

impl KernelMachine {
    /// The issued waits, found on first use by walking back from the newest
    /// wait to the first one a park reported: work in what was issued.
    fn issued(&mut self) -> Result<&mut Issued, HeapFault> {
        let found = match self.issued {
            Some(found) => found,
            None => {
                let access = self.heap.pages().clone();
                let mut found = Issued {
                    from: self.next_wait,
                    count: 0,
                };
                let mut rank = self.waits.len();
                while let Some(below) = rank.checked_sub(1) {
                    let (key, wait) = self
                        .waits
                        .rank_get(below, &access)?
                        .ok_or_else(|| HeapFault::Malformed("wait rank past the table".into()))?;
                    if !matches!(wait.phase, WaitPhase::Issued { .. }) {
                        break;
                    }
                    found = Issued {
                        from: wait_id(&key)?.0,
                        count: found.count + 1,
                    };
                    rank = below;
                }
                found
            }
        };
        Ok(self.issued.insert(found))
    }

    /// How many waits are issued and not yet reported.
    pub(super) fn issued_count(&mut self) -> Result<u64, HeapFault> {
        Ok(self.issued()?.count)
    }

    /// Counts the wait just issued. Until they are found, the issued waits
    /// are found with it.
    pub(super) fn count_issued(&mut self) -> Result<(), HeapFault> {
        if let Some(issued) = &mut self.issued {
            issued.count = issued
                .count
                .checked_add(1)
                .ok_or_else(|| HeapFault::Malformed("issued wait count overflow".into()))?;
        }
        Ok(())
    }

    /// Forgets an issued wait a cancel withdrew before any park reported it.
    pub(super) fn uncount_issued(&mut self) -> Result<(), HeapFault> {
        if let Some(issued) = &mut self.issued {
            issued.count = issued
                .count
                .checked_sub(1)
                .ok_or_else(|| HeapFault::Malformed("issued wait count underflow".into()))?;
        }
        Ok(())
    }

    /// Reports every issued wait, in identity order, admitting each: what a
    /// park hands out.
    pub(super) fn report_issued(&mut self) -> Result<Vec<crate::interface::Request>, MachineError> {
        let Issued { from, count } = *self.issued().map_err(MachineError::Heap)?;
        let mut requests = Vec::new();
        if count > 0 {
            // Each report changes Issued to Admitted. Sealing alone changes neither.
            let access = self.heap.pages().clone();
            let mut key = from.checked_sub(1).map(ordinal_key);
            while let Some((next, mut wait)) = self
                .waits
                .after(key.as_deref(), &access)
                .map_err(MachineError::Heap)?
            {
                if let WaitPhase::Issued { request } = wait.phase {
                    let id = wait_id(&next).map_err(MachineError::Heap)?;
                    requests.push(
                        self.report_request(id, wait.task, &request)
                            .map_err(MachineError::Heap)?,
                    );
                    wait.phase = WaitPhase::Admitted;
                    self.waits
                        .write(next.clone(), wait, &access)
                        .map_err(MachineError::Heap)?;
                }
                key = Some(next);
            }
        }
        self.issued = Some(Issued {
            from: self.next_wait,
            count: 0,
        });
        Ok(requests)
    }
}

fn wait_id(key: &[u8]) -> Result<WaitId, HeapFault> {
    let raw: [u8; 8] = key
        .try_into()
        .map_err(|_| HeapFault::Malformed("wait key width".into()))?;
    Ok(WaitId(u64::from_be_bytes(raw)))
}
