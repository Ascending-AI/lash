//! Current recovery-live values remain lease-qualified until exit preparation.
use super::operands::Operand;
use super::reconstruct::FrameImage;
use super::storage::Graph;
use super::*;
use lash_kernel_state::kernel_internals::logical::{
    LeaseGraphExit, LeaseValue, PublicationValue, Slot,
};

pub(super) struct Live<V: Copy> {
    pub(super) frames: Box<[FrameImage<Operand<V>>]>,
    pub(super) payload: Option<V>,
    pub(super) returning: Option<returns::ReturnImage<V>>,
    /// Held main session cells by logical owner digest.
    pub(super) sessions: Box<[([u8; 32], Operand<V>)]>,
}

pub(super) struct PublishedFrame {
    pub(super) program: CodeId,
    pub(super) invocation: u64,
    pub(super) roles: Box<[(Slot, PublicationValue)]>,
    // Physical aliases are deliberately separate from source-owned roles.
    pub(super) operands: Box<[PublicationValue]>,
    pub(super) pending: Option<PublicationValue>,
    pub(super) incoming: bool,
}
pub(super) struct Published {
    pub(super) frames: Box<[PublishedFrame]>,
    pub(super) payload: Option<PublicationValue>,
    pub(super) returning: Option<PublicationValue>,
    pub(super) sessions: Box<[([u8; 32], PublicationValue)]>,
}

impl<V: Copy> FrameImage<Operand<V>> {
    pub(super) fn materialize<G: Graph<Value = V>>(
        &self,
        graph: &G,
    ) -> Result<FrameImage, RegionError> {
        let read = |value: Operand<V>| value.materialize(graph).map_err(RegionError::Heap);
        Ok(FrameImage {
            program: self.program,
            invocation: self.invocation,
            pc: self.pc,
            continuation: self.continuation.clone(),
            roles: self
                .roles
                .iter()
                .map(|(slot, value)| Ok((*slot, read(*value)?)))
                .collect::<Result<_, RegionError>>()?,
            operands: self
                .operands
                .iter()
                .map(|value| read(*value))
                .collect::<Result<_, _>>()?,
            pending: self.pending.map(read).transpose()?,
            incoming: self.incoming,
        })
    }
}

fn publication(
    graph: &LeaseGraphExit,
    value: Operand<LeaseValue>,
    capture: bool,
) -> Result<PublicationValue, RegionError> {
    if capture {
        let Operand::Dynamic(value) = value else {
            return Err(invalid(Refused::Facts));
        };
        return match graph.value(value).map_err(RegionError::Heap)? {
            Value::ObjectRef(handle) => Ok(PublicationValue::Capture(handle)),
            _ => Err(invalid(Refused::Facts)),
        };
    }
    match value {
        Operand::Immediate(value)
            if matches!(value, Value::Int(_) | Value::Float(_) | Value::Bool(_)) =>
        {
            Ok(PublicationValue::Native(value))
        }
        Operand::Immediate(_) => Err(invalid(Refused::Facts)),
        Operand::Dynamic(value) => graph.publication(value).map_err(RegionError::Heap),
    }
}

impl Live<LeaseValue> {
    pub(super) fn publish(&self, graph: &LeaseGraphExit) -> Result<Published, RegionError> {
        Ok(Published {
            frames: self
                .frames
                .iter()
                .map(|frame| {
                    Ok(PublishedFrame {
                        program: frame.program,
                        invocation: frame.invocation,
                        roles: frame
                            .roles
                            .iter()
                            .map(|(slot, value)| {
                                Ok((
                                    *slot,
                                    publication(graph, *value, matches!(slot, Slot::Capture(_)))?,
                                ))
                            })
                            .collect::<Result<_, RegionError>>()?,
                        operands: frame
                            .operands
                            .iter()
                            .map(|value| publication(graph, *value, false))
                            .collect::<Result<_, _>>()?,
                        incoming: frame.incoming,
                        pending: frame
                            .pending
                            .map(|value| publication(graph, value, false))
                            .transpose()?,
                    })
                })
                .collect::<Result<_, RegionError>>()?,
            payload: self
                .payload
                .map(|value| publication(graph, Operand::Dynamic(value), false))
                .transpose()?,
            returning: self
                .returning
                .as_ref()
                .map(|image| publication(graph, Operand::Dynamic(image.value), false))
                .transpose()?,
            sessions: self
                .sessions
                .iter()
                .map(|(digest, value)| Ok((*digest, publication(graph, *value, false)?)))
                .collect::<Result<_, RegionError>>()?,
        })
    }
}
