"""Read one workbench session's timeline as a browser shows it.

A case runs this with the runner's Playwright interpreter. It opens the
product page for the session in headless Chromium, waits until the timeline
holds the expected number of transcript rows and the composer is idle, and
writes what each row shows: its row id, turn, kind and text, and the most
assistant replies the page ever showed at once while it loaded.

Usage: timeline.py URL SESSION ROWS OUT
"""

import json
import sys
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

# Counts the assistant rows after every timeline mutation, so a duplicate
# that a later render removes is still seen.
OBSERVER = """(() => {
    window.assistantCounts = [];
    const install = () => {
        const timeline = document.querySelector('#timeline');
        if (!timeline) return;
        new MutationObserver(() => window.assistantCounts.push(
            timeline.querySelectorAll('.message.assistant').length
        )).observe(timeline, {childList: true, subtree: true, characterData: true});
    };
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', install);
    } else {
        install();
    }
})()"""

SCRAPE = """nodes => nodes.map(node => {
    const code = node.classList.contains('code-block');
    const reasoning = node.classList.contains('reasoning');
    const text = node.querySelector('.msg-text');
    const body = node.cloneNode(true);
    body.querySelectorAll('.msg-time, .copy-btn').forEach(child => child.remove());
    return {
        id: node.dataset.transcriptRowId,
        turn: node.dataset.turnId || '',
        kind: code ? 'code' : reasoning ? 'reasoning' : 'message',
        role: node.classList.contains('assistant') ? 'assistant'
            : node.classList.contains('user') ? 'user' : 'other',
        text: code ? null : reasoning ? node.querySelector('pre').textContent
            : text ? text.textContent : body.textContent,
        code: code ? node.querySelector('.code-source').textContent : null,
    };
})"""


def main() -> None:
    url, session, rows, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), Path(sys.argv[4])
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True)
        try:
            page = browser.new_context().new_page()
            page.add_init_script(OBSERVER)
            page.goto(f"{url}/?session_id={session}", wait_until="domcontentloaded")
            expect(page.locator("#sessionId")).to_have_text(session, timeout=60000)
            page.wait_for_function("() => !document.querySelector('#send').disabled",
                                   timeout=60000)
            page.wait_for_function(
                "count => document.querySelectorAll('#timeline [data-transcript-row-id]')"
                ".length === count", arg=rows, timeout=60000)
            shown = page.locator("#timeline [data-transcript-row-id]").evaluate_all(SCRAPE)
            counts = page.evaluate("window.assistantCounts")
            out.write_text(json.dumps({
                "rows": shown,
                "most_assistants": max(counts, default=0),
            }, indent=2) + "\n")
        finally:
            browser.close()


if __name__ == "__main__":
    main()
