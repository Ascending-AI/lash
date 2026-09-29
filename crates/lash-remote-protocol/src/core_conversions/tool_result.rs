use super::*;

impl From<lash_core::facade_support::ModelToolReturnPart> for RemoteToolResultBlock {
    fn from(value: lash_core::facade_support::ModelToolReturnPart) -> Self {
        match value {
            lash_core::facade_support::ModelToolReturnPart::Text { text } => Self::Text { text },
            lash_core::facade_support::ModelToolReturnPart::Attachment(source) => {
                Self::Attachment {
                    source: Box::new(source.into()),
                }
            }
            lash_core::facade_support::ModelToolReturnPart::Retained(
                lash_core::RetainedOutput { reference, witness },
            ) => Self::Retained {
                witness,
                reference: reference.into(),
            },
        }
    }
}

impl TryFrom<RemoteToolResultBlock> for lash_core::facade_support::ModelToolReturnPart {
    type Error = RemoteProtocolError;
    fn try_from(value: RemoteToolResultBlock) -> Result<Self, Self::Error> {
        Ok(match value {
            RemoteToolResultBlock::Text { text } => Self::Text { text },
            RemoteToolResultBlock::Attachment { source } => Self::Attachment((*source).try_into()?),
            RemoteToolResultBlock::Retained { witness, reference } => {
                Self::Retained(lash_core::RetainedOutput {
                    reference: reference.try_into()?,
                    witness,
                })
            }
        })
    }
}
