# Cache capabilities are host-supplied data, not URL or model-name inference

ADR 0026 established the direction for model facts: a host attaches capability data to a
model, Lash threads it to every request, and providers consume it without trying to rediscover
it. OpenAI-compatible prompt caching had two exceptions to that rule. The provider compared
`base_url` with the canonical OpenRouter URL before emitting either `cache_control` or the
body `session_id`, then searched model ids for `claude`, `anthropic/`, or `gemini` to choose
cache placement. A proxy with identical wire behavior silently lost both features, while a
lookalike model name could select a dialect the host never authorized.

We decided both facts are explicit and live at the layer that owns them. **Cache-control
dialect is model capability data.** `ModelCapability.cache_control` is
`Option<CacheControlDialect>`, where `None` emits no Chat Completions `cache_control`,
`Anthropic` uses the canonical system/tool/explicit-breakpoint placements and supports the
one-hour TTL, and `Gemini` emits one ephemeral breakpoint with no TTL. It travels along the
existing ADR 0026 path: model spec → turn/direct/remote request → provider. The model id has
no bearing on the result.

**Session affinity is endpoint compatibility data.**
`OpenAiCompat.cache_session_affinity` defaults to disabled; when the host enables it, the
provider emits the bounded body `session_id` and call-specific `x-client-request-id` for any
base URL. `OpenAiCompat::openrouter()` is an explicit host-selected endpoint preset that
enables affinity and OpenRouter reasoning format. Choosing that preset is configuration, not
endpoint detection: the same preset works for a custom proxy, and merely using the canonical
OpenRouter URL enables nothing.

The old path is deleted. There is no URL fallback, model-name fallback, dual read, or implied
default. `base_url_is_openrouter`, `model_is_anthropic_claude`, and
`model_is_google_gemini` no longer exist. This is intentionally breaking for embedders that
construct `OpenAiCompatibleProvider` or `ModelCapability` directly: OpenRouter-like endpoints
must opt into endpoint compatibility, and each cacheable model route must carry its dialect.
An omitted field now means unsupported, not unknown-and-guessed. This preserves the useful
old wire behavior when the host supplies the equivalent data while making proxies, catalogs,
remote workers, tests, and future model launches deterministic under the same ADR 0026 seam.

## Addendum: general compatibility fields are host-supplied data

The same decision applies beyond cache capabilities. The OpenAI-compatible provider used to
classify a base URL as local when it contained `localhost`, `127.0.0.1`, `0.0.0.0`, or
`ollama`, then silently disabled optional request fields, response storage, and streaming usage.
That heuristic made the URL spelling, rather than the endpoint's declared wire contract, decide
request behavior. Proxies and LAN hosts could therefore receive the wrong request shape without
any configuration error.

The URL heuristic is deleted. `OpenAiCompat::local()` is the explicit host-selected preset that
sets `request_fields`, `store`, and `streaming_usage` to `false`, and it works for every base URL.
Without that preset (or the equivalent individual fields), all three remain enabled even for a
localhost URL. A strict local server can therefore reject an undeclared field loudly, and the
host fixes the mismatch by selecting the preset. The direct OpenAI equality retained by ADR 0072
is unchanged and remains the sole URL-derived compatibility choice.
