#!/usr/bin/env python3
"""Generate the checked-in provider-variation stream matrix.

The output is data, not an executable test oracle: every row owns one shared
normalized expectation and every dialect cell owns only provider-native wire
recordings. Keep this generator deterministic so fixture review stays useful.
"""

from __future__ import annotations

import json
import os
from pathlib import Path


ROOT = Path(__file__).resolve().parent
OUTPUT = Path(os.environ.get("LASH_PROVIDER_MATRIX_OUTPUT", ROOT / "v1" / "matrix.json"))

DIALECTS = [
    "anthropic.messages",
    "google.generate-content",
    "openai.chat-completions",
    "openai.responses",
    "codex.responses-sse",
    "codex.responses-websocket",
]


def payload(value: object) -> dict[str, object]:
    return {"json": value}


def raw(value: str) -> dict[str, str]:
    return {"raw": value}


def recording(
    case: str,
    events: list[dict[str, object]],
    *,
    close: bool = False,
    request_id: str = "matrix-request-1",
    expected_finish_reason: str | None = None,
    status: int = 200,
    body: str | None = None,
) -> dict[str, object]:
    return {
        "case": case,
        "status": status,
        "headers": [
            {"name": "content-type", "value": "text/event-stream; charset=utf-8"},
            {"name": "x-request-id", "value": request_id},
            {"name": "request-id", "value": request_id},
        ],
        "events": events,
        "body": body,
        "close_after_events": close,
        "expected_finish_reason": expected_finish_reason,
    }


def transport_error(case: str) -> dict[str, object]:
    return {
        "case": case,
        "transport_error": f"{case} before response establishment",
        "retryable": True,
    }


def failed_http_response(dialect: str) -> dict[str, object]:
    bodies = {
        "anthropic.messages": {"type": "error", "error": {"type": "invalid_request_error", "message": "matrix failure"}},
        "google.generate-content": {"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": "matrix failure"}},
        "openai.chat-completions": {"error": {"type": "invalid_request_error", "code": "matrix_failure", "message": "matrix failure"}},
        "openai.responses": {"error": {"type": "invalid_request_error", "code": "matrix_failure", "message": "matrix failure"}},
        "codex.responses-sse": {"error": {"type": "invalid_request_error", "code": "matrix_failure", "message": "matrix failure"}},
    }
    result = recording(
        "failed_response",
        [],
        request_id="matrix-failed-request",
        status=400,
        body=json.dumps(bodies[dialect], separators=(",", ":")),
    )
    if dialect == "anthropic.messages":
        result["headers"] = [
            header for header in result["headers"] if header["name"] != "x-request-id"
        ]
    return result


def buffered_response_identity() -> dict[str, object]:
    value = {
        "id": "matrix-response-1",
        "model": "matrix-served-model",
        "status": "completed",
        "output": [
            {
                "type": "message",
                "id": "matrix-message-1",
                "role": "assistant",
                "status": "completed",
                "phase": "final_answer",
                "content": [
                    {"type": "output_text", "text": "matrix identity", "annotations": []}
                ],
            }
        ],
        "usage": {
            "input_tokens": 11,
            "output_tokens": 7,
            "output_tokens_details": {"reasoning_tokens": 0},
        },
    }
    result = recording(
        "buffered_response_identity",
        [],
        expected_finish_reason="completed",
        body=json.dumps(value, separators=(",", ":")),
    )
    result["headers"][0]["value"] = "application/json"
    return result


def anthropic(
    *,
    text: str | None = None,
    stop_reason: str | None = "end_turn",
    reasoning: str | None = None,
    tool: bool = False,
    split: bool = False,
    empty_deltas: bool = False,
    empty_terminal_chunks: bool = False,
    terminal: bool = True,
) -> list[dict[str, object]]:
    events: list[dict[str, object]] = [
        payload(
            {
                "type": "message_start",
                "message": {
                    "id": "matrix-response-1",
                    "type": "message",
                    "role": "assistant",
                    "model": "matrix-served-model",
                    "content": [],
                    "stop_reason": None,
                    "usage": {
                        "input_tokens": 11,
                        "output_tokens": 0,
                        "output_tokens_details": {"thinking_tokens": 0},
                    },
                },
            }
        )
    ]
    index = 0
    if empty_deltas:
        events.extend([raw(""), payload({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": ""}})])
    if reasoning is not None:
        events.append(payload({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": ""}}))
        pieces = [reasoning[:7], reasoning[7:]] if split else [reasoning]
        for piece in pieces:
            events.append(payload({"type": "content_block_delta", "index": index, "delta": {"type": "thinking_delta", "thinking": piece}}))
        events.append(payload({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": "matrix-replay-payload"}}))
        events.append(payload({"type": "content_block_stop", "index": index}))
        index += 1
    if text is not None:
        events.append(payload({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}))
        pieces = [text[: max(1, len(text) // 2)], text[max(1, len(text) // 2) :]] if split else [text]
        for piece in pieces:
            events.append(payload({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": piece}}))
        events.append(payload({"type": "content_block_stop", "index": index}))
        index += 1
    if tool:
        events.append(payload({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": "matrix-call-1", "name": "lookup", "input": {}}}))
        pieces = ['{"q":', '"x"}'] if split else ['{"q":"x"}']
        for piece in pieces:
            events.append(payload({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": piece}}))
        events.append(payload({"type": "content_block_stop", "index": index}))
    if terminal:
        events.append(payload({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 7, "output_tokens_details": {"thinking_tokens": 2 if reasoning else 0}}}))
        if empty_terminal_chunks:
            events.extend([raw(""), payload({})])
        events.append(payload({"type": "message_stop"}))
    return events


def google(
    *,
    text: str | None = None,
    finish_reason: str | None = "STOP",
    reasoning: str | None = None,
    tool: bool = False,
    split: bool = False,
    empty_deltas: bool = False,
    empty_terminal_chunks: bool = False,
    terminal: bool = True,
) -> list[dict[str, object]]:
    def response(candidate: dict[str, object] | None = None) -> dict[str, object]:
        body: dict[str, object] = {
            "responseId": "matrix-response-1",
            "modelVersion": "matrix-served-model",
            "usageMetadata": {
                "promptTokenCount": 11,
                "candidatesTokenCount": 7,
                "thoughtsTokenCount": 2 if reasoning else 0,
            },
        }
        if candidate is not None:
            body["candidates"] = [candidate]
        return payload({"response": body})

    events: list[dict[str, object]] = []
    if empty_deltas:
        events.extend([raw(""), response({"content": {"parts": [{"text": ""}]}})])
    if reasoning is not None:
        if split:
            events.append(response({"content": {"parts": [{"text": reasoning[:7], "thought": True}]}}))
            events.append(response({"content": {"parts": [{"text": reasoning, "thought": True, "thoughtSignature": "matrix-replay-payload"}]}}))
        else:
            events.append(response({"content": {"parts": [{"text": reasoning, "thought": True, "thoughtSignature": "matrix-replay-payload"}]}}))
    if text is not None:
        events.append(response({"content": {"parts": [{"text": text}]}}))
    if tool:
        events.append(response({"content": {"parts": [{"functionCall": {"id": "matrix-call-1", "name": "lookup", "args": {"q": "x"}}, "thoughtSignature": "matrix-tool-replay"}]}}))
    if terminal:
        if empty_terminal_chunks:
            events.extend([raw(""), response({})])
        events.append(response({"finishReason": finish_reason}))
    return events


def chat(
    *,
    text: str | None = None,
    finish_reason: str | None = "stop",
    reasoning: str | None = None,
    tool: bool = False,
    split: bool = False,
    empty_deltas: bool = False,
    empty_terminal_chunks: bool = False,
    terminal: bool = True,
) -> list[dict[str, object]]:
    def chunk(choice: dict[str, object], *, usage: dict[str, object] | None = None) -> dict[str, object]:
        value: dict[str, object] = {
            "id": "matrix-response-1",
            "model": "matrix-served-model",
            "choices": [choice],
        }
        if usage is not None:
            value["usage"] = usage
        return payload(value)

    events: list[dict[str, object]] = []
    if empty_deltas:
        events.extend([raw(""), chunk({"delta": {"content": ""}})])
    if reasoning is not None:
        pieces = [reasoning[:7], reasoning[7:]] if split else [reasoning]
        events.extend(chunk({"delta": {"reasoning_content": piece}}) for piece in pieces)
    if text is not None:
        pieces = [text[: max(1, len(text) // 2)], text[max(1, len(text) // 2) :]] if split else [text]
        events.extend(chunk({"delta": {"content": piece}}) for piece in pieces)
    if tool:
        pieces = ['{"q":', '"x"}'] if split else ['{"q":"x"}']
        events.append(chunk({"delta": {"tool_calls": [{"index": 0, "id": "matrix-call-1", "type": "function", "function": {"name": "lookup", "arguments": pieces[0]}}]}}))
        for piece in pieces[1:]:
            events.append(chunk({"delta": {"tool_calls": [{"index": 0, "function": {"arguments": piece}}]}}))
        events.append(chunk({"delta": {"reasoning_details": [{"type": "reasoning.encrypted", "id": "matrix-call-1", "data": "matrix-tool-replay"}]}}))
    if terminal:
        if empty_terminal_chunks:
            events.extend([raw(""), chunk({"delta": {}})])
        events.append(
            chunk(
                {"delta": {}, "finish_reason": finish_reason, "native_finish_reason": finish_reason},
                usage={
                    "prompt_tokens": 11,
                    "completion_tokens": 7,
                    "completion_tokens_details": {"reasoning_tokens": 2 if reasoning else 0},
                },
            )
        )
        events.append(raw("[DONE]"))
    return events


def responses(
    *,
    text: str | None = None,
    incomplete_reason: str | None = None,
    reasoning: str | None = None,
    tool: bool = False,
    split: bool = False,
    empty_deltas: bool = False,
    empty_terminal_chunks: bool = False,
    terminal: bool = True,
    response_id: str = "matrix-response-1",
    done_sentinel: bool = True,
) -> list[dict[str, object]]:
    events: list[dict[str, object]] = [
        payload({"type": "response.created", "response": {"id": response_id, "model": "matrix-served-model", "status": "in_progress"}})
    ]
    output: list[dict[str, object]] = []
    output_index = 0
    if empty_deltas:
        events.extend([raw(""), payload({"type": "response.output_text.delta", "output_index": 0, "delta": ""})])
    if reasoning is not None:
        item = {
            "type": "reasoning",
            "id": "matrix-reasoning-1",
            "summary": [{"type": "summary_text", "text": reasoning}],
            "encrypted_content": "matrix-replay-payload",
        }
        events.append(payload({"type": "response.output_item.added", "output_index": output_index, "item": {"type": "reasoning", "id": "matrix-reasoning-1"}}))
        pieces = [reasoning[:7], reasoning[7:]] if split else [reasoning]
        events.append(payload({"type": "response.reasoning_summary_part.added", "output_index": output_index, "part": {"type": "summary_text", "text": ""}}))
        for piece in pieces:
            events.append(payload({"type": "response.reasoning_summary_text.delta", "output_index": output_index, "delta": piece}))
        events.append(payload({"type": "response.reasoning_summary_part.done", "output_index": output_index, "part": {"type": "summary_text", "text": reasoning}}))
        events.append(payload({"type": "response.output_item.done", "output_index": output_index, "item": item}))
        output.append(item)
        output_index += 1
    if text is not None:
        item = {
            "type": "message",
            "id": "matrix-message-1",
            "role": "assistant",
            "status": "completed",
            "phase": "final_answer",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }
        events.append(payload({"type": "response.output_item.added", "output_index": output_index, "item": {"type": "message", "id": "matrix-message-1", "status": "in_progress", "phase": "final_answer", "content": []}}))
        pieces = [text[: max(1, len(text) // 2)], text[max(1, len(text) // 2) :]] if split else [text]
        for piece in pieces:
            events.append(payload({"type": "response.output_text.delta", "output_index": output_index, "item_id": "matrix-message-1", "delta": piece}))
        events.append(payload({"type": "response.output_item.done", "output_index": output_index, "item": item}))
        output.append(item)
        output_index += 1
    if tool:
        item = {
            "type": "function_call",
            "id": "matrix-function-1",
            "call_id": "matrix-call-1",
            "name": "lookup",
            "arguments": '{"q":"x"}',
            "status": "completed",
        }
        events.append(payload({"type": "response.output_item.added", "output_index": output_index, "item": {**item, "arguments": ""}}))
        pieces = ['{"q":', '"x"}'] if split else ['{"q":"x"}']
        for piece in pieces:
            events.append(payload({"type": "response.function_call_arguments.delta", "output_index": output_index, "item_id": "matrix-function-1", "delta": piece}))
        events.append(payload({"type": "response.function_call_arguments.done", "output_index": output_index, "item_id": "matrix-function-1", "name": "lookup", "arguments": '{"q":"x"}'}))
        events.append(payload({"type": "response.output_item.done", "output_index": output_index, "item": item}))
        output.append(item)
    if terminal:
        if empty_terminal_chunks:
            events.extend([raw(""), payload({"type": "response.output_text.delta", "output_index": 0, "delta": ""})])
        status = "incomplete" if incomplete_reason else "completed"
        response_value: dict[str, object] = {
            "id": response_id,
            "model": "matrix-served-model",
            "status": status,
            "output": output,
            "usage": {
                "input_tokens": 11,
                "output_tokens": 7,
                "output_tokens_details": {"reasoning_tokens": 2 if reasoning else 0},
            },
        }
        if incomplete_reason:
            response_value["incomplete_details"] = {"reason": incomplete_reason}
        event_type = "response.incomplete" if incomplete_reason else "response.completed"
        events.append(payload({"type": event_type, "response": response_value}))
        if done_sentinel:
            events.append(raw("[DONE]"))
    return events


BUILDERS = {
    "anthropic.messages": anthropic,
    "google.generate-content": google,
    "openai.chat-completions": chat,
    "openai.responses": responses,
    "codex.responses-sse": responses,
    "codex.responses-websocket": responses,
}


def cell(streams: list[dict[str, object]]) -> dict[str, object]:
    return {"kind": "recorded_streams", "recordings": streams}


def stream_for(
    dialect: str,
    case: str,
    *,
    close: bool = False,
    request_id: str = "matrix-request-1",
    **kwargs: object,
) -> dict[str, object]:
    if dialect == "codex.responses-websocket":
        kwargs.setdefault("done_sentinel", False)
    terminal = kwargs.get("terminal", True)
    expected_finish_reason: str | None = None
    if terminal:
        if dialect == "anthropic.messages":
            expected_finish_reason = str(kwargs.get("stop_reason", "end_turn"))
        elif dialect == "google.generate-content":
            expected_finish_reason = str(kwargs.get("finish_reason", "STOP"))
        elif dialect == "openai.chat-completions":
            expected_finish_reason = str(kwargs.get("finish_reason", "stop"))
        else:
            expected_finish_reason = str(kwargs.get("incomplete_reason") or "completed")
    return recording(
        case,
        BUILDERS[dialect](**kwargs),
        close=close,
        request_id=request_id,
        expected_finish_reason=expected_finish_reason,
    )


def matrix() -> dict[str, object]:
    rows: list[dict[str, object]] = []
    rows.append(
        {
            "variation": "stop_consumed",
            "expectation": {"kind": "stop_boundary", "literal_present": False},
            "cells": {
                "anthropic.messages": {"kind": "provider_wire_scripts", "paths": ["../../variations/anthropic.messages-stop-consumed.json"]},
                "google.generate-content": {"kind": "provider_wire_scripts", "paths": ["../../variations/google.generate-content-stop-consumed.json"]},
                "openai.chat-completions": {"kind": "provider_wire_scripts", "paths": ["../../variations/openai-compatible.chat-stop-consumed.json"]},
                "openai.responses": {
                    "kind": "not_applicable",
                    "reason": "Responses has no provider wire stop-sequence field",
                    "recordings": [stream_for("openai.responses", "unsupported_stop", text='<lashlang>\nfinish "settled"\n</lashlang>\n')],
                },
                "codex.responses-sse": {
                    "kind": "not_applicable",
                    "reason": "Codex Responses has no provider wire stop-sequence field",
                    "recordings": [stream_for("codex.responses-sse", "unsupported_stop", text='<lashlang>\nfinish "settled"\n</lashlang>\n')],
                },
                "codex.responses-websocket": {
                    "kind": "not_applicable",
                    "reason": "Codex Responses WebSocket has no provider wire stop-sequence field",
                    "recordings": [stream_for("codex.responses-websocket", "unsupported_stop", text='<lashlang>\nfinish "settled"\n</lashlang>\n')],
                },
            },
        }
    )
    for dialect in ("openai.responses", "codex.responses-sse", "codex.responses-websocket"):
        unsupported = rows[0]["cells"][dialect]["recordings"][0]
        unsupported["request_match"] = {
            "body": {
                "stop": {"present": False},
                "stop_sequences": {"present": False},
            },
            "headers": {},
        }
    literal_cells: dict[str, object] = {
        "anthropic.messages": {"kind": "provider_wire_scripts", "paths": ["../../variations/anthropic.messages-literal-present.json"]},
        "google.generate-content": {"kind": "provider_wire_scripts", "paths": ["../../variations/google.generate-content-literal-present.json"]},
        "openai.chat-completions": {"kind": "provider_wire_scripts", "paths": ["../../variations/openai-compatible.chat-literal-present.json"]},
    }
    for dialect in DIALECTS[3:]:
        literal_cells[dialect] = cell([stream_for(dialect, "literal_present", text='<lashlang>\nfinish "settled"\n</lashlang>\n')])
    rows.append(
        {
            "variation": "literal_present",
            "expectation": {"kind": "stop_boundary", "literal_present": True},
            "cells": literal_cells,
        }
    )

    outcome_cells: dict[str, object] = {}
    for dialect in DIALECTS:
        terminal_args = {
            "anthropic.messages": {
                "stop": {"stop_reason": "end_turn"},
                "tool": {"stop_reason": "tool_use"},
                "length": {"stop_reason": "max_tokens"},
                "content_filter": {"stop_reason": "refusal"},
            },
            "google.generate-content": {
                "stop": {"finish_reason": "STOP"},
                "tool": {"finish_reason": "STOP"},
                "length": {"finish_reason": "MAX_TOKENS"},
                "content_filter": {"finish_reason": "SAFETY"},
            },
            "openai.chat-completions": {
                "stop": {"finish_reason": "stop"},
                "tool": {"finish_reason": "tool_calls"},
                "length": {"finish_reason": "length"},
                "content_filter": {"finish_reason": "content_filter"},
            },
            "openai.responses": {
                "stop": {},
                "tool": {},
                "length": {"incomplete_reason": "max_output_tokens"},
                "content_filter": {"incomplete_reason": "content_filter"},
            },
            "codex.responses-sse": {
                "stop": {},
                "tool": {},
                "length": {"incomplete_reason": "max_output_tokens"},
                "content_filter": {"incomplete_reason": "content_filter"},
            },
            "codex.responses-websocket": {
                "stop": {},
                "tool": {},
                "length": {"incomplete_reason": "max_output_tokens"},
                "content_filter": {"incomplete_reason": "content_filter"},
            },
        }[dialect]
        outcome_cells[dialect] = cell(
            [
                stream_for(dialect, "stop", text="matrix stop", **terminal_args["stop"]),
                stream_for(dialect, "tool", tool=True, **terminal_args["tool"]),
                stream_for(dialect, "length", text="matrix partial", **terminal_args["length"]),
                stream_for(dialect, "content_filter", text="matrix filtered", **terminal_args["content_filter"]),
            ]
        )
    rows.append(
        {
            "variation": "equivalent_outcome_mapping",
            "expectation": {"kind": "equivalent_outcome_mapping"},
            "cells": outcome_cells,
        }
    )

    missing_cells = {}
    for dialect in DIALECTS:
        builder_args: dict[str, object] = {"text": "matrix partial", "terminal": False}
        close = dialect == "codex.responses-websocket"
        missing_cells[dialect] = cell([recording("missing_terminal", BUILDERS[dialect](**builder_args), close=close)])
    rows.append(
        {
            "variation": "missing_terminal_evidence",
            "expectation": {"kind": "missing_terminal_evidence"},
            "cells": missing_cells,
        }
    )

    shape_cells = {}
    for dialect in DIALECTS:
        tool_terminal = {
            "anthropic.messages": {"stop_reason": "tool_use"},
            "google.generate-content": {"finish_reason": "STOP"},
            "openai.chat-completions": {"finish_reason": "tool_calls"},
        }.get(dialect, {})
        shape_cells[dialect] = cell(
            [
                stream_for(dialect, "empty_deltas", text="matrix shape", empty_deltas=True),
                stream_for(
                    dialect,
                    "empty_terminal_chunks",
                    text="matrix shape",
                    empty_terminal_chunks=True,
                ),
                stream_for(dialect, "split_reasoning_tool", reasoning="matrix reasoning", tool=True, split=True, **tool_terminal),
            ]
        )
    rows.append(
        {
            "variation": "event_shape_variation",
            "expectation": {"kind": "event_shape_variation"},
            "cells": shape_cells,
        }
    )

    reasoning_cells = {
        dialect: cell([stream_for(dialect, "reasoning_rendered", reasoning="matrix reasoning", text="matrix answer", split=True)])
        for dialect in DIALECTS
    }
    rows.append(
        {
            "variation": "reasoning_rendered_path",
            "expectation": {"kind": "reasoning_rendered_path"},
            "cells": reasoning_cells,
        }
    )

    identity_cells = {}
    for dialect in DIALECTS:
        success = stream_for(dialect, "response_identity", text="matrix identity")
        if dialect == "codex.responses-websocket":
            failed_response = {
                "type": "response.failed",
                "response": {
                    "id": "matrix-failed-response",
                    "model": "matrix-served-model",
                    "status": "failed",
                    "error": {
                        "type": "invalid_request_error",
                        "code": "matrix_failure",
                        "message": "matrix failure",
                    },
                    "usage": {
                        "input_tokens": 11,
                        "output_tokens": 0,
                        "output_tokens_details": {"reasoning_tokens": 0},
                    },
                },
            }
            failure = recording("failed_response", [payload(failed_response)])
        else:
            failure = failed_http_response(dialect)
        recordings = [success, failure]
        if dialect in {"openai.responses", "codex.responses-sse"}:
            recordings.append(buffered_response_identity())
        identity_cells[dialect] = cell(recordings)
    rows.append(
        {
            "variation": "response_identity_execution_evidence",
            "expectation": {"kind": "response_identity_execution_evidence"},
            "cells": identity_cells,
        }
    )

    retry_cells = {}
    for dialect in DIALECTS:
        success = stream_for(dialect, "attempt_3_success", text="matrix retry settled")
        early_attempts = []
        for attempt in (1, 2):
            if dialect == "codex.responses-websocket":
                early_attempts.append(
                    stream_for(
                        dialect,
                        f"attempt_{attempt}",
                        close=True,
                        request_id=f"matrix-request-attempt-{attempt}",
                        terminal=False,
                        response_id=f"matrix-response-attempt-{attempt}",
                    )
                )
            else:
                early_attempts.append(
                    recording(
                        f"attempt_{attempt}",
                        [],
                        request_id=f"matrix-request-attempt-{attempt}",
                    )
                )
        retry_cells[dialect] = cell([*early_attempts, success])
    rows.append(
        {
            "variation": "multi_reset_retry_sequence",
            "expectation": {"kind": "multi_reset_retry_sequence"},
            "cells": retry_cells,
        }
    )

    return {
        "schema": "lash.provider-variation-matrix.v1",
        "dialects": DIALECTS,
        "rows": rows,
    }


def main() -> None:
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(json.dumps(matrix(), indent=2, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
