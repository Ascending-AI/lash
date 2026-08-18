# Slack-clone deterministic full-host companion

This scripted harness follows the repository-wide [runbook rules](../RULES.md).

This is the executable, hermetic companion to the manual judged
[`slack-clone-bot`](../slack-clone-bot/runbook.md) runbook, whose Phase 3M
absorbed the former `slack-clone-mcp-client-depth` scenario once this harness
was shown to cover its whole scorecard. Run it with:

```bash
just slack-clone-full-host-e2e
```

It acquires the repository worktree gate and takes offsets `+35`, `+36`, and
`+37` of that worktree's 50-port block for its platform, bot, and HTTP MCP
server — three consecutive ports that stay inside the block, since the port
lock is per slot and an offset past `+49` would be the next slot's `+0`. It
keeps state outside the checkout, and always tears down its platform, bot,
stdio MCP child, HTTP MCP server, and two headless Chromium contexts. If the
derived slot is occupied, `LASH_GATE_SLOT_OVERRIDE` selects a different
worktree gate block. `LASH_SLACK_CLONE_E2E_ARTIFACT_DIR` selects the evidence
directory. There is no model call or token dependency.

The scorecard at `scorecard.json` reconciles four named layers at every
applicable checkpoint: rendered DOM in both browser contexts, platform HTTP API
plus SQLite truth, bot ledger/session SQLite truth, and JSONL trace plus typed
disposition logs. Screenshots and a complete four-layer extract accompany every
checkpoint.

## Exact automated boundary

| Manual runbook cell | Deterministic CI coverage |
| --- | --- |
| Bot phases 0-2 | Boot/identity/silence and two ambient admissions, with no turn |
| Bot phase 3 | One mention, folded context, dropped twin, one native `list_channels` tool and one reply |
| Bot phase 3T | Retained thread fork, inherited root, both-direction post-fork isolation, two thread replies |
| Bot phase 3T root recall | The child's first answer names the thread root and not the later room mention; the child's first provider request carries the host's thread-root seed exactly once, on a line of its own — queued text inputs concatenate with no separator, so a seed behind copied pre-root context would start mid-line and stop being a label. Inheritance is read from the fork lineage's ancestor chain, never from the child's own graph rows — `fork_at` writes no nodes into the child, so an isolation gate written against those rows passes vacuously |
| Bot phase 4 | Exact event-envelope redelivery, durable delivery increment, no new turn or reply |
| Bot phase 5 | Provider-entry/accepted-stage kill, durable claim evidence, real live-lease deferral, timed reclaim/retry, one recovered reply, live DOM mutation history |
| Bot phase 5T | **NONE** — the deterministic suite proves the same kill/recovery machinery on the channel route; child-route mid-turn recovery remains judged/manual and has focused Rust coverage |
| Bot phase 6 | **NONE** — platform-outbox crash recovery is orthogonal to the FIG-1341 bot full-host acceptance and remains judged/manual plus focused Rust coverage |
| Bot phase 7 | Both independent contexts reload to the same top-level API/database projection |
| Bot phase 3M (runtime integration attach/detach) | An operator attaches the HTTP-served MCP server over the bot's admin API mid-run: the status row reports connected with its advertised tools, the next turn calls its binary-content tool and the bytes land in the host attachment store as a `stored` attachment, detaching leaves the operator view holding only the stdio server the bot booted with, and the following turn's provider request no longer offers the tool. **NOT covered:** that a real model chooses the newly-offered tool unprompted and reports its absence honestly after detach — that is model behaviour, which is why phase 3M keeps a judged half |
| Bot phase 3M (MCP client depth) | One rendered turn invokes sampling, form elicitation, URL elicitation/completion, and roots over the bundled stdio child; four committed results and four exact trace attempts. **NOT covered:** that sampling is served by the bot's configured real provider — a scripted provider has no model id to name, which is why phase 3M survives as a judged step |

The deterministic provider is compiled only by the `e2e` Cargo feature and
requires the explicit `SLACK_CLONE_E2E_PROVIDER=scripted-v1` selector. Omitting
the selector preserves the normal OpenRouter requirement even in an E2E-feature
build.

## Deliberate RED proof

The implementation report records five temporary source mutations, one at each
evidence-producing boundary — including the runtime-attach checkpoint, whose bot
layer was reproduced red by configuring the attached HTTP server without binary
content attachments — and the checkpoint that rejected each mutation.
Those mutations are reverted after proof and are not shipped as runtime test
switches.
