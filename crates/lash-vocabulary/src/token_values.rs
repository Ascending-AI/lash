//! Canonical encodings of host-owned leaves.
use crate::LimitCause;
use lash_kernel_value::tokens::*;

#[doc(hidden)]
impl CodecVariant for LimitCause {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::ExecutionSlice => 0,
            Self::ExecutionTotal => 1,
            Self::WaitDeadline => 2,
        }
    }
}

use crate::providers::PluginCallbackIdentity;

impl TokenCodec for PluginCallbackIdentity {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.text(&self.owner);
        out.text(&self.key);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            owner: input.text()?.to_owned(),
            key: input.text()?.to_owned(),
        })
    }
}
