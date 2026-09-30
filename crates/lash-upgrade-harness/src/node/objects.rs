//! Direct calls to lash's own Restate handlers, as one build makes them.
//!
//! `call` sends one [`Call`] through ingress with this build's wire range
//! (or a range the leg names, to play a caller no build is) and reports the
//! [`Reply`] or the typed refusal the handler answered. `sweep` runs lash's
//! own object sweep (ADR 0115 §3.2, FIG-4041) over every object family,
//! printing one report line per object as its `upgrade` answers, so a leg
//! can crash it mid-sweep.

use std::io::Write as _;

use anyhow::{Result, anyhow, bail};
use clap::{Args, ValueEnum};
use lash_core_store::compat::CompatRefusal;
use lash_restate::{
    Call, RESTATE_WIRE, RestateAdminClient, RestateConnection, RestateHttpError,
    RestateIngressClient, RestateNamespace, RestateObjectUpgradeTarget, SweepReport, VersionRange,
};
use serde::{Deserialize, Serialize};

use super::RestateArgs;
use crate::identity::BuildLabel;

/// Whether the target is a virtual object or a workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TargetKind {
    Object,
    Workflow,
}

#[derive(Clone, Debug, Args)]
pub struct CallArgs {
    #[command(flatten)]
    pub restate: RestateArgs,
    /// The lash service, by its unqualified name (`EffectGroupIndex`).
    #[arg(long)]
    pub service: String,
    #[arg(long, value_enum, default_value_t = TargetKind::Object)]
    pub kind: TargetKind,
    #[arg(long)]
    pub key: String,
    #[arg(long)]
    pub handler: String,
    /// The call's body as JSON.
    #[arg(long, default_value = "null")]
    pub body: String,
    /// The wire range the call states, `MIN..MAX`; this build's own range
    /// when absent.
    #[arg(long, value_parser = parse_range)]
    pub wire: Option<VersionRange>,
}

#[derive(Clone, Debug, Args)]
pub struct SweepArgs {
    #[command(flatten)]
    pub restate: RestateArgs,
}

/// `MIN..MAX` as a [`VersionRange`].
pub fn parse_range(value: &str) -> Result<VersionRange, String> {
    let (min, max) = value
        .split_once("..")
        .ok_or_else(|| format!("`{value}` is not MIN..MAX"))?;
    let parse = |bound: &str| {
        bound
            .trim()
            .parse::<u32>()
            .map_err(|error| format!("`{bound}`: {error}"))
    };
    VersionRange::new(parse(min)?, parse(max)?).map_err(|error| error.to_string())
}

/// A lash handler's typed refusal, as its terminal error carries it
/// (ADR 0115 §3.1–3.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error")]
pub enum HandlerRefusal {
    /// The caller reads no wire version the handler's build answers.
    #[serde(rename = "lash.wire_unsupported")]
    WireUnsupported {
        local: VersionRange,
        peer: VersionRange,
    },
    /// The object's `_compat` record refuses the handler's build.
    #[serde(rename = "lash.incompatible")]
    Incompatible { refusal: CompatRefusal },
}

impl HandlerRefusal {
    /// The refusal a terminal error's message carries, if it is one.
    pub fn in_message(message: &str) -> Option<Self> {
        let start = message.find(r#"{"error":"lash."#)?;
        serde_json::Deserializer::from_str(&message[start..])
            .into_iter::<Self>()
            .next()?
            .ok()
    }
}

/// What one call answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CallOutcome {
    /// The handler answered a [`Reply`](lash_restate::Reply) at `wire`.
    Replied { wire: u32, body: serde_json::Value },
    /// The handler refused the call, typed, before any state.
    Refused {
        status: u16,
        refusal: HandlerRefusal,
    },
    /// Anything else the call ended with.
    Failed { message: String },
}

/// What `call` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallReport {
    /// The build that made the call.
    pub caller: BuildLabel,
    /// The wire range the call stated.
    pub wire: VersionRange,
    pub outcome: CallOutcome,
}

fn ingress(restate: &RestateArgs) -> RestateIngressClient {
    RestateIngressClient::new(RestateConnection::new(restate.ingress_url.clone()))
}

fn namespace(restate: &RestateArgs) -> Result<RestateNamespace> {
    RestateNamespace::new(&restate.namespace).map_err(|error| anyhow!("namespace: {error}"))
}

/// The outcome of one ingress call to a lash handler.
fn outcome(answer: Result<serde_json::Value, RestateHttpError>) -> CallOutcome {
    match answer {
        Ok(reply) => match (
            reply.get("wire").and_then(serde_json::Value::as_u64),
            reply.get("body"),
        ) {
            (Some(wire), Some(body)) => CallOutcome::Replied {
                wire: u32::try_from(wire).unwrap_or(u32::MAX),
                body: body.clone(),
            },
            _ => CallOutcome::Failed {
                message: format!("the handler answered no Reply envelope: {reply}"),
            },
        },
        Err(RestateHttpError::Status { status, body, .. }) => {
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| body.clone());
            match HandlerRefusal::in_message(&message) {
                Some(refusal) => CallOutcome::Refused { status, refusal },
                None => CallOutcome::Failed {
                    message: format!("status {status}: {message}"),
                },
            }
        }
        Err(error) => CallOutcome::Failed {
            message: error.to_string(),
        },
    }
}

/// Send one call to a lash handler.
async fn call_handler(
    client: &RestateIngressClient,
    service: &str,
    kind: TargetKind,
    key: &str,
    handler: &str,
    call: &Call<serde_json::Value>,
) -> CallOutcome {
    let answer = match kind {
        TargetKind::Object => {
            client
                .call_object_json::<_, serde_json::Value>(service, key, handler, call)
                .await
        }
        TargetKind::Workflow => {
            client
                .call_workflow_json::<_, serde_json::Value>(service, key, handler, call)
                .await
        }
    };
    outcome(answer)
}

pub(super) async fn call(args: CallArgs) -> Result<CallReport> {
    let body: serde_json::Value =
        serde_json::from_str(&args.body).map_err(|error| anyhow!("--body: {error}"))?;
    let wire = args.wire.unwrap_or(RESTATE_WIRE);
    let service = namespace(&args.restate)?.service_name(&args.service);
    let outcome = call_handler(
        &ingress(&args.restate),
        &service,
        args.kind,
        &args.key,
        &args.handler,
        &Call { wire, body },
    )
    .await;
    Ok(CallReport {
        caller: BuildLabel::current(),
        wire,
        outcome,
    })
}

/// Run lash's object sweep, printing one [`SweptObject`] per object as its
/// `upgrade` answers.
pub(super) async fn sweep(args: SweepArgs) -> Result<SweepReport> {
    let target = RestateObjectUpgradeTarget::new(
        RestateAdminClient::new(RestateConnection::new(args.restate.admin_url.clone())),
        ingress(&args.restate),
        namespace(&args.restate)?,
    );
    let mut failed = None;
    let report = lash_restate::sweep_objects(&target, |object| {
        let mut stdout = std::io::stdout().lock();
        let written = serde_json::to_writer(&mut stdout, object)
            .map_err(anyhow::Error::from)
            .and_then(|()| stdout.write_all(b"\n").map_err(anyhow::Error::from))
            .and_then(|()| stdout.flush().map_err(anyhow::Error::from));
        if let Err(error) = written {
            failed.get_or_insert(error);
        }
    })
    .await
    .map_err(|error| anyhow!("the object sweep: {error}"))?;
    if let Some(error) = failed {
        bail!("printing the sweep: {error}");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::{HandlerRefusal, VersionRange, parse_range};

    #[test]
    fn ranges_parse_as_min_dot_dot_max() {
        assert_eq!(parse_range("1..2"), Ok(VersionRange::between(1, 2)));
        assert!(parse_range("2..1").is_err());
        assert!(parse_range("2").is_err());
    }

    #[test]
    fn a_refusal_is_read_back_from_its_terminal_message() {
        let message = r#"Cannot decode input payload: {"error":"lash.wire_unsupported","local":{"min":1,"max":1},"peer":{"min":2,"max":2}}"#;
        assert_eq!(
            HandlerRefusal::in_message(message),
            Some(HandlerRefusal::WireUnsupported {
                local: VersionRange::exactly(1),
                peer: VersionRange::exactly(2),
            })
        );
        assert_eq!(HandlerRefusal::in_message("a plain failure"), None);
    }
}
