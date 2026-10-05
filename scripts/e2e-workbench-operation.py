#!/usr/bin/env python3
"""S17: a same-origin browser follows a dropped-caller operation Run and reads durable admission."""
from __future__ import annotations

import argparse
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--run", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    from playwright.sync_api import sync_playwright

    paths = [
        f"/api/e2e/sessions/{args.session}/operations/{args.run}",
        f"/api/e2e/sessions/{args.session}/admission",
    ]
    with sync_playwright() as playwright:
        chrome = playwright.chromium.launch()
        try:
            page = chrome.new_page()
            response = page.goto(args.base_url, wait_until="domcontentloaded")
            assert response is not None and response.ok, "workbench page did not load"
            follow, admission = page.evaluate(
                """async ([followPath, admissionPath]) => {
                    const get = async (path) => {
                        const response = await fetch(path);
                        if (!response.ok) throw new Error(`${path}: ${response.status}`);
                        return response.json();
                    };
                    return [await get(followPath), await get(admissionPath)];
                }""",
                paths,
            )
        finally:
            chrome.close()
    args.out.write_text(
        json.dumps({"follow": follow, "admission": admission}, indent=2) + "\n"
    )


if __name__ == "__main__":
    main()
