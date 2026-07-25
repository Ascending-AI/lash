#!/usr/bin/env python3
"""Tests for release_notes.py (pure functions; no git fixtures needed)."""

from __future__ import annotations

import tempfile
from pathlib import Path

import release_notes


ALPHA_112_SQUASH_BODY = """\
Production recoverable-chat host reference and conformance gate (FIG-610) (#127)

* Add recoverable chat observation contract

Expose snapshot-first recovery updates with stable redelivery identity, replay-gap replacement, terminal replacement, and disconnect-safe subscription semantics. Add real observation API conformance gates for each contract edge.

Release-Notes:
- Lash now exposes a recoverable_chat observation surface for snapshot-first chat hosts, including stable event identity, replay-gap snapshots, terminal replacement, and explicit observer-disconnect semantics.

* Harden the workbench recoverable chat projection

Split Lash observations from durable ordered product events, resume the remote observation cursor directly, deduplicate stable product and observation identities, and resynchronize on lag or terminal replacement. Retract provisional attempts, shield raw failures, consume typed turn-input application evidence, expose the authorization seam, and extend the real owner-restart gate.

* Document recoverable chat host responsibilities

Describe the separated Lash and product lanes, snapshot-first resumption, typed turn-input application evidence, projection replacement rules, same-turn recovery versus retry-copy semantics, uncertain tool effects, and the pluggable authorization seam.

* Make recoverable chat handoff lossless

Capture the read view and cursor from one installed runtime observation, clear subscription-scoped deduplication after replay discontinuity, and bound retained identities at terminal replacement. Add deterministic regressions for the snapshot publication race and replay-store cursor reuse.

Release-Notes:
- Recoverable chat snapshots now pair the committed read view with its exact observation cursor, and replay-gap recovery discards pre-gap event identities before continuing.

* Make workbench recovery convergent

Fence asynchronous state recovery, scope cursors and deduplication to a session, merge canonical transcript history with product-only rows, and give every cancel settlement a unique product identity. Route observation through the recoverable-chat seam and keep internal diagnostics out of HTTP responses.

Replace projection string checks with route, turn-output, provider-failure, and executable browser-reducer gates, including production-handler mutation proof.
"""


def extracted(body: str) -> list[str]:
    """Use the new API, with a pre-fix fallback for regression verification."""
    if hasattr(release_notes, "extract_notes"):
        return release_notes.extract_notes(body)
    note = release_notes.extract_note(body)
    return [note] if note else []


def test_extract_notes_block_form_preserves_markdown() -> None:
    body = (
        "Subject\n\nRelease-Notes:\n"
        "Fixed: First paragraph.\n\n- Detail one\n- Detail two\n"
        "* Squash subject\n\nInternal prose.\n"
    )
    assert extracted(body) == [
        "Fixed: First paragraph.\n\n- Detail one\n- Detail two"
    ]


def test_extract_notes_inline_form_stops_at_squash_subject() -> None:
    body = (
        "Subject\n\n"
        "Release-Notes: Added: Single line note.\n"
        "* Squash subject\n\nInternal prose.\n"
    )
    assert extracted(body) == ["Added: Single line note."]


def test_extract_notes_multiple_markers_preserve_order() -> None:
    body = (
        "* First change\n\n"
        "Release-Notes: Fixed: First public note.\n"
        "* Second change\n\nImplementation details.\n\n"
        "Release-Notes:\nChanged: Second public note.\n\n- Detail.\n"
        "* Third change\n\nMore implementation details.\n"
    )
    assert extracted(body) == [
        "Fixed: First public note.",
        "Changed: Second public note.\n\n- Detail.",
    ]


def test_alpha_112_squash_fixture_extracts_only_both_notes() -> None:
    assert extracted(ALPHA_112_SQUASH_BODY) == [
        (
            "- Lash now exposes a recoverable_chat observation surface for "
            "snapshot-first chat hosts, including stable event identity, "
            "replay-gap snapshots, terminal replacement, and explicit "
            "observer-disconnect semantics."
        ),
        (
            "- Recoverable chat snapshots now pair the committed read view with "
            "its exact observation cursor, and replay-gap recovery discards "
            "pre-gap event identities before continuing."
        ),
    ]


def test_extract_notes_absent_or_empty() -> None:
    assert extracted("Subject\n\nNo marker here.\n") == []
    assert extracted("Subject\n\nRelease-Notes:\n\n") == []


def test_marker_must_start_the_line() -> None:
    body = "Subject\n\nSee the Release-Notes: convention for details.\n"
    assert extracted(body) == []


def test_render_notes_groups_categories_in_publication_order() -> None:
    notes = [
        "fixed: Repair the first issue.",
        "Added: Add the new surface.",
        "BREAKING: Replace the old API.",
        "Internal: Exercise the release-note gate.",
        "Changed: Adjust the behavior.",
        "Fixed: Repair the second issue.",
    ]
    assert release_notes.render_notes(notes) == """\
## Breaking

Replace the old API.

## Added

Add the new surface.

## Fixed

Repair the first issue.

Repair the second issue.

## Changed

Adjust the behavior.

## Internal

Exercise the release-note gate."""


def test_render_notes_accepts_legacy_dash_categories() -> None:
    notes = [
        "Fixed - repair legacy behavior.",
        "Changed - preserve historical syntax.",
    ]
    assert release_notes.render_notes(notes) == """\
## Fixed

repair legacy behavior.

## Changed

preserve historical syntax."""


def test_render_notes_keeps_uncategorized_notes_under_other() -> None:
    notes = ["A historical uncategorized note."]
    assert release_notes.render_notes(notes) == """\
## Other

A historical uncategorized note."""


def test_pr_rule_requires_note_for_product_changes() -> None:
    errors = release_notes.validate_pr_notes(["crates/lash/src/lib.rs"], [])
    assert errors == [
        "product changes under `crates/` require at least one categorized "
        "release note"
    ]


def test_pr_rule_rejects_uncategorized_notes_even_when_exempt() -> None:
    errors = release_notes.validate_pr_notes(
        ["scripts/release_notes.py"], ["An uncategorized note."]
    )
    assert errors == [
        "every release note must start with one of: Breaking:, Added:, Fixed:, "
        "Changed:, Internal:"
    ]


def test_internal_category_is_the_no_public_note_escape_hatch() -> None:
    errors = release_notes.validate_pr_notes(
        ["crates/lash/src/lib.rs"],
        ["Internal: This product change has no public-facing effect."],
    )
    assert errors == []


def test_exempt_pr_without_notes_is_valid() -> None:
    assert release_notes.validate_pr_notes(["scripts/release_notes.py"], []) == []


def test_pr_summary_is_clear_when_exempt_pr_has_no_notes() -> None:
    with tempfile.TemporaryDirectory() as directory:
        summary = Path(directory) / "summary.md"
        release_notes.write_pr_summary(summary, "")
        assert summary.read_text(encoding="utf-8") == (
            "## Release notes preview (this PR)\n\n"
            "No release notes in this PR.\n"
        )


def main() -> int:
    failures = 0
    for name, test in sorted(globals().items()):
        if name.startswith("test_") and callable(test):
            try:
                test()
                print(f"ok   {name}")
            except Exception as err:  # noqa: BLE001 - tiny dependency-free harness
                failures += 1
                print(f"FAIL {name}: {type(err).__name__}: {err}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
