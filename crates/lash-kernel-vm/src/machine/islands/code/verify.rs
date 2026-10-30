//! Packet projections are checked against the canonical source region table
//! and the source producer's independently derived call continuation roles.

use super::*;
use crate::compile::islands::{Refused, verify_close};

pub(super) trait Closes {
    fn verify(&self, stream: &Stream) -> Result<(), Refused>;
}

impl Closes for crate::compile::expr::Close {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        verify_close(Some(self), stream)
    }
}

impl<T: Closes> Closes for Option<T> {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        self.as_ref().map_or(Ok(()), |value| value.verify(stream))
    }
}

impl<T: Closes> Closes for Box<T> {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        (**self).verify(stream)
    }
}

macro_rules! close_site {
    ($($site:ty),* $(,)?) => { $(
        impl Closes for $site {
            fn verify(&self, stream: &Stream) -> Result<(), Refused> {
                self.close.verify(stream)
            }
        }
    )* };
}
close_site!(
    OperatorSite,
    HeapSite,
    NativeSite,
    CallSite,
    WriteSite,
    UnitSite
);

impl Closes for sites::Native {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        self.site.verify(stream)
    }
}

impl Closes for sites::Kind {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        self.site.verify(stream)
    }
}

impl Closes for sites::Heap {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        self.site.verify(stream)?;
        self.resident.verify(stream)
    }
}

impl Closes for compiled::Format {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        self.join.verify(stream)?;
        self.append.verify(stream)?;
        self.write.verify(stream)
    }
}

impl Closes for compiled::Update {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        self.read.verify(stream)?;
        self.scalar.verify(stream)?;
        self.write.verify(stream)
    }
}

impl Closes for KindTest {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        self.kind.verify(stream)?;
        self.same.verify(stream)
    }
}

impl Closes for Destination {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        match self {
            Self::Register => Ok(()),
            Self::Write(site) => site.verify(stream),
        }
    }
}

impl Closes for HeapKernel {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        match self {
            Self::Read { to, .. } => to.verify(stream),
            Self::Write { .. } => Ok(()),
        }
    }
}

impl Closes for AccessSite {
    fn verify(&self, stream: &Stream) -> Result<(), Refused> {
        self.kind.verify(stream)
    }
}

// ReadSite authenticates a resident lookup position; its canonical charge
// close is a separate packet, not part of this cache descriptor.
impl Closes for crate::compile::ReadSite {
    fn verify(&self, _stream: &Stream) -> Result<(), Refused> {
        Ok(())
    }
}

impl Code {
    pub(super) fn verify_roles(&self, stream: &Stream) -> Result<(), Refused> {
        for (packet, recovery) in self.packets.iter().zip(&self.recovery) {
            if packet.opcode != Opcode::Call {
                continue;
            }
            let site = &self.sites.call[packet.x as usize].site;
            let Some(act) = site.act else {
                continue;
            };
            let nested = stream.nested.get(act.ix()).ok_or(Refused::Calls)?;
            for role in &nested.roles {
                let retained = recovery.live.contains(&role.reg.0)
                    || site
                        .inputs
                        .as_ref()
                        .is_some_and(|inputs| inputs.reads().any(|read| read.reg() == role.reg.0));
                if !retained {
                    return Err(Refused::Calls);
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::machine::islands) fn corrupt_call_role(&mut self, stream: &Stream) -> bool {
        for (packet, recovery) in self.packets.iter().zip(&mut self.recovery) {
            if packet.opcode != Opcode::Call {
                continue;
            }
            let site = &self.sites.call[packet.x as usize].site;
            let Some(role) = site
                .act
                .and_then(|act| stream.nested[act.ix()].roles.first())
            else {
                continue;
            };
            if recovery.live.contains(&role.reg.0) && site.inputs.is_none() {
                recovery.live = recovery
                    .live
                    .iter()
                    .copied()
                    .filter(|reg| *reg != role.reg.0)
                    .collect();
                return true;
            }
        }
        false
    }
}
