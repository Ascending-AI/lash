use super::*;

impl From<lash_core::provider::LlmProfileRequestDefaults> for RemoteLlmProfileRequestDefaults {
    fn from(value: lash_core::provider::LlmProfileRequestDefaults) -> Self {
        let lash_core::provider::LlmProfileRequestDefaults {
            expose_thinking,
            max_output_tokens,
            cache_retention,
            response_metadata_headers,
            response_metadata_body_paths,
        } = value;
        Self {
            expose_thinking,
            max_output_tokens,
            cache_retention: match cache_retention {
                lash_core::provider::CacheRetention::None => RemoteCacheRetention::None,
                lash_core::provider::CacheRetention::Short => RemoteCacheRetention::Short,
                lash_core::provider::CacheRetention::Long => RemoteCacheRetention::Long,
            },
            response_metadata_headers,
            response_metadata_body_paths,
        }
    }
}

impl From<RemoteLlmProfileRequestDefaults> for lash_core::provider::LlmProfileRequestDefaults {
    fn from(value: RemoteLlmProfileRequestDefaults) -> Self {
        let RemoteLlmProfileRequestDefaults {
            expose_thinking,
            max_output_tokens,
            cache_retention,
            response_metadata_headers,
            response_metadata_body_paths,
        } = value;
        Self {
            expose_thinking,
            max_output_tokens,
            cache_retention: match cache_retention {
                RemoteCacheRetention::None => lash_core::provider::CacheRetention::None,
                RemoteCacheRetention::Short => lash_core::provider::CacheRetention::Short,
                RemoteCacheRetention::Long => lash_core::provider::CacheRetention::Long,
            },
            response_metadata_headers,
            response_metadata_body_paths,
        }
    }
}
