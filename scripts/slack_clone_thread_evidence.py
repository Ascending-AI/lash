"""Pure evidence helpers shared by the slack-clone full-host driver.

They live outside the driver so they can be exercised without a browser: both
encode a model of Lash fork semantics that a scripted provider hid, and a model
is worth testing directly.
"""

from __future__ import annotations

from typing import Any


# Kept byte-identical with `THREAD_ROOT_SEED_PREFIX`
# (examples/slack-clone/src/bot/threads.rs). The host seeds the thread root into
# the child because Lash forks at a committed boundary and cannot know which of
# the inherited messages a thread hangs from.
THREAD_ROOT_SEED_PREFIX = "Thread root (the channel message this thread replies to): "


def select_thread_root(messages: list[dict[str, Any]], marker: str) -> dict[str, Any]:
    """Pick the thread root by author identity, with the marker only narrowing.

    Selecting it as "the row whose text contains the marker" is wrong the moment
    the bot's own channel reply quotes the fact it was asked to recall — which is
    exactly what the phase-3 mention asks for. The root of a thread that tests
    inheritance has to be human-authored, so that is the selector; an ambiguous
    match is an error rather than a silent first-match.
    """
    candidates = [row for row in messages if marker in row["text"] and not row.get("is_bot")]
    if len(candidates) != 1:
        raise AssertionError(
            f"expected exactly one human-authored row carrying {marker}, got {len(candidates)}"
        )
    return candidates[0]


def seed_label_line_starts(value: Any, label: str = THREAD_ROOT_SEED_PREFIX) -> tuple[int, int]:
    """Count occurrences of `label` in a decoded record, and how many start a line.

    Queued text inputs concatenate into one user message with no separator, so a
    seed enqueued behind copied context runs out of the tail of that line unless
    the host writes the break itself. A mid-line label is not a label, and the
    two counts differing is exactly that failure. Walks decoded values rather
    than encoded JSON so real newlines are compared, not `\\n` escapes.
    """
    total = 0
    at_line_start = 0
    stack = [value]
    while stack:
        node = stack.pop()
        if isinstance(node, str):
            index = node.find(label)
            while index != -1:
                total += 1
                if index == 0 or node[index - 1] == "\n":
                    at_line_start += 1
                index = node.find(label, index + 1)
        elif isinstance(node, dict):
            stack.extend(node.values())
        elif isinstance(node, list):
            stack.extend(node)
    return total, at_line_start


def inherited_prefix_nodes(
    nodes: list[dict[str, Any]],
    lineage: list[dict[str, Any]],
    thread_id: str,
    ancestor_id: str,
) -> list[dict[str, Any]]:
    """The ancestor nodes a fork actually inherits, newest last.

    `fork_at` adds a session head at a retained boundary *without writing graph
    nodes*, so the child's own `graph_nodes` rows never hold ancestor content.
    Reading inheritance out of those rows models a fork as node copying: the
    inclusion check then fails on a correct fork, and — worse — the isolation
    check passes vacuously, because post-fork ancestor traffic could never have
    appeared there whether the boundary was right or not. The honest prefix is
    the ancestor chain from the recorded fork node back to the ancestor root.
    """
    row = next(
        (
            row
            for row in lineage
            if row["session_id"] == thread_id and row["ancestor_session_id"] == ancestor_id
        ),
        None,
    )
    if row is None or not row["fork_node_id"]:
        raise AssertionError(f"no retained fork lineage from {ancestor_id} to {thread_id}")
    by_id = {node["node_id"]: node for node in nodes if node["session_id"] == ancestor_id}
    chain: list[dict[str, Any]] = []
    seen: set[str] = set()
    cursor: str | None = row["fork_node_id"]
    while cursor is not None:
        if cursor in seen:
            raise AssertionError(f"ancestor graph cycles at {cursor}")
        seen.add(cursor)
        node = by_id.get(cursor)
        if node is None:
            raise AssertionError(f"fork lineage names {cursor}, absent from the ancestor graph")
        chain.append(node)
        cursor = node["parent_node_id"]
    chain.reverse()
    return chain
