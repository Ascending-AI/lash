# Buffered HTTP response budgets

Every `read_http_body_bytes` and `read_http_body_text` call requires a raw byte
limit. The reader checks already buffered bodies before returning or decoding
them. For streamed bodies it checks the whole next chunk against the remaining
budget before reserving storage or appending bytes. Exactly the limit succeeds
at EOF; the next byte refuses the response. A zero limit permits an empty body.
Content-Length does not determine admission or allocation.

Providers select the limit with `ProviderOptions.response_body_bytes`. `None`
uses 16 MiB. The limit covers non-SSE completion bodies, HTTP errors, and
auxiliary reads such as generation lookups and upload responses. SSE keeps its
separate event and total budgets.

An excess body returns `HttpFailureContext::ResponseBodyTooLarge` with the limit
and the minimum observed size, and the failure code
`lash:http_response_body_too_large`. The refusal is not retryable. Provider
error handling preserves the read failure
instead of substituting an empty response body. Successful and error bodies
share the same read boundary; neither JSON parsing nor diagnostic truncation
runs on excess bytes.

The accumulator grows geometrically with explicit allocation requests capped
at the selected budget. Cumulative accumulator requests are bounded by three
times the budget plus fixed bookkeeping. Lossy UTF-8 decoding can expand each
raw byte to three output bytes. The pinned-toolchain allocation witness bounds
cumulative reader and text allocation requests, including reallocations, by
`12 * limit + 2048` bytes, excluding transport poll futures. The fragmented
fixture also counts its boxed poll futures, allowing another 32 bytes per poll
on the pinned toolchain. Refusing an oversized first chunk or buffered body
requests at most 2048 bytes of diagnostic/bookkeeping storage, without copying
or decoding the body. The fixture also checks that a giant chunk arriving after
an exact-limit prefix cannot cause an excess append or another stream poll.

These counters measure allocation requests, not RSS. They exclude chunks or
buffers already owned by the transport, allocator overhead, and subsequent
caller JSON parsing. A custom transport remains responsible for the memory it
allocates before handing chunks or a buffered body to the reader. The reader
never uses a caller-supplied Content-Length to reserve that memory.

The laws cover buffered and streamed inputs, advisory lengths, native HTTP
fixtures and both OpenAI endpoints.
