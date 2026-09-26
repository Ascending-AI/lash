//! The process requests a caller sends into a running process workflow carry
//! the journal generation they were written for
//! ([`RESTATE_PROCESS_JOURNAL_VERSION`]).
//!
//! Generation 4 (FIG-3607) changed each request's shape: a process is named
//! by its minted id alone, with no incarnation. A request is stamped on the
//! way out, and on the way in its generation is read from the raw payload
//! before its shape is decoded, so a request of a retired generation is
//! refused by generation rather than by whichever field it lacks.

use lash_core::{AwaitEventKey, CancelRequest, ProcessAwaitOutput, ProcessId};
use serde::{Deserialize, Serialize};

use super::admission::{RESTATE_PROCESS_JOURNAL_VERSION, decode_stamped_request};

/// Declares the stamped wire form of one request type: a private mirror that
/// carries the generation, the `From` that stamps it, and the probe-first
/// `TryFrom` that reads it.
macro_rules! stamped_request {
    ($request:ident, $wire:ident, $kind:literal { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub(crate) struct $wire {
            $($field: $ty,)*
            journal_version: u32,
        }

        impl From<super::$request> for $wire {
            fn from(request: super::$request) -> Self {
                let super::$request { $($field),* } = request;
                Self {
                    $($field,)*
                    journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
                }
            }
        }

        impl TryFrom<serde_json::Value> for super::$request {
            type Error = String;

            fn try_from(payload: serde_json::Value) -> Result<Self, Self::Error> {
                let $wire { $($field,)* journal_version: _ } =
                    decode_stamped_request::<$wire>($kind, payload)
                        .map_err(|error| error.message().to_string())?;
                Ok(Self { $($field),* })
            }
        }
    };
}

/// The cancel request carries its generation as a field of its own (the
/// handlers read it), so only its decode is probed here.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StampedCancelRequest {
    process_id: ProcessId,
    request: CancelRequest,
    journal_version: u32,
}

impl TryFrom<serde_json::Value> for super::RestateProcessCancelRequest {
    type Error = String;

    fn try_from(payload: serde_json::Value) -> Result<Self, Self::Error> {
        let StampedCancelRequest {
            process_id,
            request,
            journal_version,
        } = decode_stamped_request::<StampedCancelRequest>("process cancel request", payload)
            .map_err(|error| error.message().to_string())?;
        Ok(Self {
            process_id,
            request,
            journal_version,
        })
    }
}

stamped_request!(RestateProcessCompleteRequest, StampedCompleteRequest, "process terminal completion" {
    process_id: ProcessId,
    output: ProcessAwaitOutput,
});

stamped_request!(RestateProcessAwaitRequest, StampedAwaitRequest, "process await request" {
    process_id: ProcessId,
});

/// The attach request lives in `process_attach`, beside its workflow.
pub(crate) mod attach {
    use super::*;
    use crate::process_attach::RestateProcessAttachRequest;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct StampedAttachRequest {
        process_id: ProcessId,
        key: AwaitEventKey,
        journal_version: u32,
    }

    impl From<RestateProcessAttachRequest> for StampedAttachRequest {
        fn from(request: RestateProcessAttachRequest) -> Self {
            let RestateProcessAttachRequest { process_id, key } = request;
            Self {
                process_id,
                key,
                journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
            }
        }
    }

    impl TryFrom<serde_json::Value> for RestateProcessAttachRequest {
        type Error = String;

        fn try_from(payload: serde_json::Value) -> Result<Self, Self::Error> {
            let StampedAttachRequest {
                process_id,
                key,
                journal_version: _,
            } = decode_stamped_request::<StampedAttachRequest>("process attach request", payload)
                .map_err(|error| error.message().to_string())?;
            Ok(Self { process_id, key })
        }
    }
}
