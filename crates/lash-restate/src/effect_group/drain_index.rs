//! A generation's derived directory of groups that may still owe a seat.
//! Registration precedes the authoritative group commit. Directory entries
//! never decide drain state; the operator reads each listed group's record.

use super::*;
use lash_core::engine::BuildGeneration;

pub(crate) const ENTRY_PREFIX: &str = "group/";

fn entry_key(group: &str) -> String {
    format!("{ENTRY_PREFIX}{group}")
}

#[restate_sdk::object]
#[name = "EffectGroupDrainIndex"]
pub(crate) trait EffectGroupDrainIndex {
    async fn register(call: Call<String>) -> HandlerResult<Reply<()>>;
    async fn upgrade(call: Call<()>) -> HandlerResult<Reply<ObjectUpgradeResponse>>;
}

#[derive(Clone, Debug, Default)]
pub(crate) struct EffectGroupDrainIndexImpl {
    pub(crate) fleet: FleetView,
}

impl EffectGroupDrainIndex for EffectGroupDrainIndexImpl {
    async fn register(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<String>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, group) = call.open()?;
        BuildGeneration::parse(ctx.key()).map_err(|error| TerminalError::new(error.to_string()))?;
        let object = object_state::admit_exclusive(
            &ctx,
            &EFFECT_GROUP_STATE_FAMILY,
            self.fleet.fleet_format(),
        )
        .await?;
        object_state::set_stamped(&ctx, &entry_key(&group), object.writer, ());
        Ok(Reply::at(wire, ()))
    }

    async fn upgrade(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<ObjectUpgradeResponse>> {
        let (wire, ()) = call.open()?;
        let response = object_state::upgrade_object(
            &ctx,
            &EFFECT_GROUP_STATE_FAMILY,
            self.fleet.fleet_format(),
        )
        .await?;
        Ok(Reply::at(wire, response))
    }
}

pub(super) async fn register(
    ctx: &ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    record: &EffectGroupStateRecord,
) -> HandlerResult<()> {
    let generation =
        crate::services::generation_lane_of(&record.dispatch_route).ok_or_else(|| {
            TerminalError::new("a group drain registration needs its recorded generation")
        })?;
    namespace
        .effect_group_drain_index(ctx, generation.as_str())
        .register(ctx.key().to_string())
        .call()
        .await
        .map_err(|error| {
            let cause = RuntimeEffectControllerError::new(
                RuntimeErrorCode::EngineAwaitEventAwait,
                format!(
                    "register effect group {} for generation {} before commitment: {error}",
                    ctx.key(),
                    generation
                ),
            );
            restate_sdk::errors::HandlerError::from(std::io::Error::other(
                serde_json::to_string(&cause).unwrap_or_else(|_| cause.to_string()),
            ))
        })?;
    Ok(())
}
