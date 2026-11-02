//! Final boundary claims stay under the ledger owner. Interior source events
//! do not enter this representation; composed transfers retain only the entry
//! owners consumed and the final surviving exact values.
use super::*;

#[derive(Default)]
pub(in crate::paged::logical) struct RootProof {
    inputs: Vec<Owner>,
    outputs: Vec<(Owner, PublicationValue)>,
}
impl RootProof {
    pub(in crate::paged::logical) fn new(
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Self {
        let mut proof = Self::default();
        proof.append(inputs, outputs);
        proof
    }

    pub(in crate::paged::logical) fn append(
        &mut self,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) {
        let values: Vec<_> = outputs
            .iter()
            .map(|(owner, value)| {
                let value = match value {
                    RootValue::Existing(from) => self
                        .outputs
                        .iter()
                        .find(|(owner, _)| owner == from)
                        .map_or_else(
                            || PublicationValue::from(*value),
                            |(_, value)| value.clone(),
                        ),
                    _ => PublicationValue::from(*value),
                };
                (*owner, value)
            })
            .collect();
        for input in inputs {
            if let Some(index) = self.outputs.iter().position(|(owner, _)| owner == input) {
                self.outputs.remove(index);
            } else if !self.inputs.contains(input) {
                self.inputs.push(*input);
            }
        }
        for (owner, value) in values {
            if let Some((_, old)) = self.outputs.iter_mut().find(|(root, _)| *root == owner) {
                *old = value;
            } else {
                self.outputs.push((owner, value));
            }
        }
    }

    pub(super) fn matches(&self, inputs: &[Owner], outputs: &[(Owner, RootValue<'_>)]) -> bool {
        if inputs
            .iter()
            .enumerate()
            .any(|(index, owner)| inputs[..index].contains(owner))
            || outputs.iter().enumerate().any(|(index, (owner, _))| {
                outputs[..index].iter().any(|(earlier, _)| earlier == owner)
            })
        {
            return false;
        }
        let proposed = Self::new(inputs, outputs);
        self.inputs == proposed.inputs
            && self.outputs.len() == proposed.outputs.len()
            && self
                .outputs
                .iter()
                .zip(proposed.outputs)
                .all(|((owner, value), (root, actual))| *owner == root && value.same(&actual))
    }
}
