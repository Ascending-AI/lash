#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["playwright==1.62.0"]
# ///
"""Browser oracle and failure capture for the live Slack-clone nonce swap."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from playwright.sync_api import expect, sync_playwright


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--artifact-dir", type=Path, required=True)
    parser.add_argument("--channel-name")
    parser.add_argument("--nonce-a")
    parser.add_argument("--nonce-b")
    parser.add_argument("--capture-only", action="store_true")
    args = parser.parse_args()
    if not args.capture_only and (
        not args.channel_name or not args.nonce_a or not args.nonce_b
    ):
        parser.error(
            "--channel-name, --nonce-a and --nonce-b are required unless --capture-only is set"
        )
    return args


def main() -> int:
    args = parse_args()
    args.artifact_dir.mkdir(parents=True, exist_ok=True)
    evidence: dict[str, object] = {
        "oracle": "rendered DOM contains both per-attempt nonces",
        "nonce_a_present": False,
        "nonce_b_present": False,
        "messages": [],
    }
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 1440, "height": 1000})
        try:
            page.goto(args.base_url)
            expect(page.locator("#namePicker")).to_be_visible(timeout=10_000)
            page.locator("#nameInput").fill("Live acceptance")
            page.locator("#nameForm button").click()
            expect(page.locator("#whoami")).to_have_text("Live acceptance", timeout=10_000)
            channel_name = args.channel_name or "general"
            page.locator("#channels .channel", has_text=f"# {channel_name}").click()
            expect(page.locator("#channelName")).to_have_text(
                f"# {channel_name}", timeout=10_000
            )
            if not args.capture_only:
                expect(page.locator("#stream")).to_contain_text(args.nonce_a, timeout=15_000)
                expect(page.locator("#stream")).to_contain_text(args.nonce_b, timeout=15_000)
                evidence["nonce_a_present"] = True
                evidence["nonce_b_present"] = True
            evidence["messages"] = page.locator("#stream .msg").evaluate_all(
                "els => els.map(e => ({ts: e.dataset.ts, text: e.querySelector('.msg-body').textContent}))"
            )
            page.screenshot(path=args.artifact_dir / "ui.png", full_page=True)
            (args.artifact_dir / "ui-dom.html").write_text(page.content(), encoding="utf-8")
            (args.artifact_dir / "ui-oracle.json").write_text(
                json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
            )
            return 0
        except Exception as error:
            evidence["error"] = str(error)
            try:
                evidence["messages"] = page.locator("#stream .msg").evaluate_all(
                    "els => els.map(e => ({ts: e.dataset.ts, text: e.querySelector('.msg-body').textContent}))"
                )
                page.screenshot(path=args.artifact_dir / "failure-ui.png", full_page=True)
                (args.artifact_dir / "failure-ui-dom.html").write_text(
                    page.content(), encoding="utf-8"
                )
            finally:
                (args.artifact_dir / "ui-oracle.json").write_text(
                    json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
                )
            print(f"UI oracle failed: {error}")
            return 1
        finally:
            browser.close()


if __name__ == "__main__":
    raise SystemExit(main())
