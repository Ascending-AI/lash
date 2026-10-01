//! The `EffectGroupPayload` object: a group's shared payload bytes and their
//! retirement fence, each stored under the stamped object-state envelope.

use super::*;

const PAYLOAD_STATE_KEY: &str = "effect-group/v1/payload";
const PAYLOAD_RETIRED_KEY: &str = "effect-group/v1/retired";

/// The stored format the payload object stamps into its `effect-group/v1/`
/// values (FIG-3814), the payload bytes and the retirement fence, and the
/// family format of every `EffectGroupPayload` object's `_compat` record
/// (ADR 0115 §3.2). Bump it when a stored shape under those keys changes,
/// and register the previous format's lift in
/// `lash_core::store::RECORD_UPCASTERS`.
///
/// version_guard(
///     items(PAYLOAD_STATE_KEY, PAYLOAD_RETIRED_KEY, EFFECT_GROUP_PAYLOAD_FORMATS),
///     shapes(path = "crates/lash-restate/src/object_state.rs", cover(StampedValue)),
/// )
#[cfg(not(feature = "synthetic-next"))]
pub const EFFECT_GROUP_PAYLOAD_FORMAT_VERSION: u16 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the family to format 2 with
/// format 1's shape; its `upgrade` handler rewrites each object after
/// finalize.
#[cfg(feature = "synthetic-next")]
pub const EFFECT_GROUP_PAYLOAD_FORMAT_VERSION: u16 = 2;

/// The payload object's stored-format table: the family's registered surface.
pub(crate) const EFFECT_GROUP_PAYLOAD_FORMATS: StoredValueFormats = StoredValueFormats {
    what: "effect-group payload",
    surface: lash_core::surface_format!(EFFECT_GROUP_PAYLOAD_FORMAT_VERSION),
};

/// The object family whose `_compat` record every handler admits first
/// (ADR 0115 §3.2).
pub(crate) const EFFECT_GROUP_PAYLOAD_FAMILY: ObjectFamily = ObjectFamily {
    component: lash_core_store::compat::ComponentId::RESTATE_EFFECT_GROUP_PAYLOAD,
    formats: &EFFECT_GROUP_PAYLOAD_FORMATS,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupPayloadPutResponse {
    Written,
    Duplicate,
    Conflict,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupPayloadGetResponse {
    Stored { bytes: Vec<u8> },
    Missing,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupPayloadPutRequest {
    pub bytes: Vec<u8>,
}

/// A group's successful result bytes, one object per group child. Every
/// handler takes a [`Call`] and answers a [`Reply`], and reads the object's
/// `_compat` record before any other state (ADR 0115 §3).
#[restate_sdk::object]
#[name = "EffectGroupPayload"]
pub(crate) trait EffectGroupPayload {
    async fn put(
        call: Call<EffectGroupPayloadPutRequest>,
    ) -> HandlerResult<Reply<EffectGroupPayloadPutResponse>>;
    #[shared]
    async fn get(call: Call<()>) -> HandlerResult<Reply<EffectGroupPayloadGetResponse>>;
    async fn retire(call: Call<()>) -> HandlerResult<Reply<()>>;
    async fn delete_bytes(call: Call<()>) -> HandlerResult<Reply<()>>;
    /// Rewrite the payload at the newest family format once finalize has
    /// moved the fleet to it, and raise its `_compat` (ADR 0115 §3.2,
    /// FIG-4041): the object sweep's step.
    async fn upgrade(call: Call<()>) -> HandlerResult<Reply<ObjectUpgradeResponse>>;
}

/// The [`EffectGroupPayload`] handlers: they call no other service, so
/// they are the same in every namespace. They stamp what they write at the
/// format the deployment's fleet epoch selects.
#[derive(Clone, Debug, Default)]
pub(crate) struct EffectGroupPayloadImpl {
    fleet: FleetView,
}

impl EffectGroupPayloadImpl {
    pub(crate) fn new(fleet: FleetView) -> Self {
        Self { fleet }
    }
}

impl EffectGroupPayload for EffectGroupPayloadImpl {
    async fn put(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupPayloadPutRequest>,
    ) -> HandlerResult<Reply<EffectGroupPayloadPutResponse>> {
        let (wire, request) = call.open()?;
        let object = object_state::admit_exclusive(
            &ctx,
            &EFFECT_GROUP_PAYLOAD_FAMILY,
            self.fleet.fleet_format(),
        )
        .await?;
        if object_state::get_stamped::<bool>(
            &ctx,
            PAYLOAD_RETIRED_KEY,
            &EFFECT_GROUP_PAYLOAD_FORMATS,
        )
        .await?
        .unwrap_or(false)
        {
            return Ok(Reply::at(wire, EffectGroupPayloadPutResponse::Retired));
        }
        let response = match object_state::get_stamped::<Vec<u8>>(
            &ctx,
            PAYLOAD_STATE_KEY,
            &EFFECT_GROUP_PAYLOAD_FORMATS,
        )
        .await?
        {
            None => {
                object_state::set_stamped(&ctx, PAYLOAD_STATE_KEY, object.writer, request.bytes);
                EffectGroupPayloadPutResponse::Written
            }
            Some(existing) if existing == request.bytes => EffectGroupPayloadPutResponse::Duplicate,
            Some(_) => EffectGroupPayloadPutResponse::Conflict,
        };
        Ok(Reply::at(wire, response))
    }

    async fn get(
        &self,
        ctx: SharedObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<EffectGroupPayloadGetResponse>> {
        let (wire, ()) = call.open()?;
        object_state::admit_shared(&ctx, &EFFECT_GROUP_PAYLOAD_FAMILY).await?;
        if object_state::get_stamped_shared::<bool>(
            &ctx,
            PAYLOAD_RETIRED_KEY,
            &EFFECT_GROUP_PAYLOAD_FORMATS,
        )
        .await?
        .unwrap_or(false)
        {
            return Ok(Reply::at(wire, EffectGroupPayloadGetResponse::Retired));
        }
        Ok(Reply::at(
            wire,
            match object_state::get_stamped_shared::<Vec<u8>>(
                &ctx,
                PAYLOAD_STATE_KEY,
                &EFFECT_GROUP_PAYLOAD_FORMATS,
            )
            .await?
            {
                Some(bytes) => EffectGroupPayloadGetResponse::Stored { bytes },
                None => EffectGroupPayloadGetResponse::Missing,
            },
        ))
    }

    async fn retire(&self, ctx: ObjectContext<'_>, call: Call<()>) -> HandlerResult<Reply<()>> {
        let (wire, ()) = call.open()?;
        let object = object_state::admit_exclusive(
            &ctx,
            &EFFECT_GROUP_PAYLOAD_FAMILY,
            self.fleet.fleet_format(),
        )
        .await?;
        object_state::set_stamped(&ctx, PAYLOAD_RETIRED_KEY, object.writer, true);
        Ok(Reply::at(wire, ()))
    }

    async fn delete_bytes(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, ()) = call.open()?;
        object_state::admit_exclusive(
            &ctx,
            &EFFECT_GROUP_PAYLOAD_FAMILY,
            self.fleet.fleet_format(),
        )
        .await?;
        ctx.clear(PAYLOAD_STATE_KEY);
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
            &EFFECT_GROUP_PAYLOAD_FAMILY,
            self.fleet.fleet_format(),
        )
        .await?;
        Ok(Reply::at(wire, response))
    }
}
