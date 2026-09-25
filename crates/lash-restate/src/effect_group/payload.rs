//! The `EffectGroupPayload` object: a group's shared payload bytes and their
//! retirement fence, each stored under the stamped object-state envelope.

use super::*;

const PAYLOAD_STATE_KEY: &str = "effect-group/v1/payload";
const PAYLOAD_RETIRED_KEY: &str = "effect-group/v1/retired";

/// The stored format the payload object stamps into its `effect-group/v1/`
/// values (FIG-3814): the payload bytes and the retirement fence. Bump it
/// when a stored shape under those keys changes; the previous format reads
/// through the N-1 upcaster slot in [`EFFECT_GROUP_PAYLOAD_FORMATS`].
pub const EFFECT_GROUP_PAYLOAD_FORMAT_VERSION: u16 = 1;
/// The payload object's stored-format table: the current stamp, plus the
/// N-1 upcaster hooks (empty while the first stamped layout is the baseline).
pub(crate) const EFFECT_GROUP_PAYLOAD_FORMATS: StoredValueFormats = StoredValueFormats {
    what: "effect-group payload",
    current: EFFECT_GROUP_PAYLOAD_FORMAT_VERSION,
    upcast_n1: &[],
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct EffectGroupPayload;

#[restate_sdk::object(name = "EffectGroupPayload")]
impl EffectGroupPayload {
    #[handler]
    async fn put(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupPayloadPutRequest>,
    ) -> HandlerResult<Json<EffectGroupPayloadPutResponse>> {
        if object_state::get_stamped::<bool>(
            &ctx,
            PAYLOAD_RETIRED_KEY,
            &EFFECT_GROUP_PAYLOAD_FORMATS,
        )
        .await?
        .unwrap_or(false)
        {
            return Ok(Json(EffectGroupPayloadPutResponse::Retired));
        }
        let response = match object_state::get_stamped::<Vec<u8>>(
            &ctx,
            PAYLOAD_STATE_KEY,
            &EFFECT_GROUP_PAYLOAD_FORMATS,
        )
        .await?
        {
            None => {
                object_state::set_stamped(
                    &ctx,
                    PAYLOAD_STATE_KEY,
                    &EFFECT_GROUP_PAYLOAD_FORMATS,
                    request.bytes,
                );
                EffectGroupPayloadPutResponse::Written
            }
            Some(existing) if existing == request.bytes => EffectGroupPayloadPutResponse::Duplicate,
            Some(_) => EffectGroupPayloadPutResponse::Conflict,
        };
        Ok(Json(response))
    }

    #[handler]
    async fn get(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> HandlerResult<Json<EffectGroupPayloadGetResponse>> {
        if object_state::get_stamped_shared::<bool>(
            &ctx,
            PAYLOAD_RETIRED_KEY,
            &EFFECT_GROUP_PAYLOAD_FORMATS,
        )
        .await?
        .unwrap_or(false)
        {
            return Ok(Json(EffectGroupPayloadGetResponse::Retired));
        }
        Ok(Json(
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

    #[handler]
    async fn retire(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<()>> {
        object_state::set_stamped(
            &ctx,
            PAYLOAD_RETIRED_KEY,
            &EFFECT_GROUP_PAYLOAD_FORMATS,
            true,
        );
        Ok(Json(()))
    }

    #[handler]
    async fn delete_bytes(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<()>> {
        ctx.clear(PAYLOAD_STATE_KEY);
        Ok(Json(()))
    }
}
