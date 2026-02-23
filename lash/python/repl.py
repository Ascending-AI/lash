import builtins
import json
import io
import re
import sys
import os
import ast
import inspect
import traceback
import types
import typing
import asyncio
import uuid
import warnings
import math
from collections import Counter, defaultdict
from enum import StrEnum

import dill

# Suppress SyntaxWarnings from LLM-generated code (e.g. unrecognized escape
# sequences like "\|"). These warnings write directly to stderr via C-level
# fprintf and corrupt the TUI's alternate screen. We must redirect the actual
# fd 2 to /dev/null around parse/compile calls since Python-level
# warnings.filterwarnings is bypassed by the C tokenizer.
warnings.filterwarnings("ignore", category=SyntaxWarning)

_devnull_fd = os.open(os.devnull, os.O_WRONLY)

def _mute_stderr():
    """Redirect fd 2 to /dev/null, return saved fd."""
    saved = os.dup(2)
    os.dup2(_devnull_fd, 2)
    return saved

def _unmute_stderr(saved):
    """Restore fd 2 from saved fd."""
    os.dup2(saved, 2)
    os.close(saved)


# ─── Turn History ───

class ToolName(StrEnum):
    READ_FILE = "read_file"
    WRITE_FILE = "write_file"
    EDIT_FILE = "edit_file"
    FIND_REPLACE = "find_replace"
    GLOB = "glob"
    GREP = "grep"
    LS = "ls"
    SHELL = "shell"
    WEB_SEARCH = "web_search"
    FETCH_URL = "fetch_url"
    AGENT_CALL = "agent_call"
    TASKS = "tasks"
    SKILLS = "skills"
    HASHLINE = "hashline"
    OTHER = "other"

_TOOL_NAME_MAP = {v.value: v for v in ToolName}


class Intelligence(StrEnum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"


# Tools whose "path" arg counts as a file read
_READ_TOOLS = {ToolName.READ_FILE, ToolName.GLOB, ToolName.GREP}
# Tools whose "path" arg counts as a file write
_WRITE_TOOLS = {ToolName.WRITE_FILE, ToolName.EDIT_FILE, ToolName.FIND_REPLACE}


class ToolCall:
    __slots__ = ("tool", "args", "result", "success", "duration_ms")

    def __init__(self, tool, args, result, success, duration_ms):
        self.tool = tool
        self.args = args
        self.result = result
        self.success = success
        self.duration_ms = duration_ms

    def __repr__(self):
        sym = "\u2713" if self.success else "\u2717"
        parts = [f"ToolCall({self.tool.value}"]
        path = self.args.get("path") or self.args.get("pattern")
        if path:
            parts.append(f' path="{path}"')
        parts.append(f" {sym} {self.duration_ms}ms)")
        return "".join(parts)


class Turn:
    __slots__ = ("index", "user_message", "prose", "code", "output", "error", "tool_calls", "files_read", "files_written")

    def __init__(self, index, user_message, prose, code, output, error, tool_calls, files_read, files_written):
        self.index = index
        self.user_message = user_message
        self.prose = prose
        self.code = code
        self.output = output
        self.error = error
        self.tool_calls = tool_calls
        self.files_read = files_read
        self.files_written = files_written

    def __repr__(self):
        tc = len(self.tool_calls)
        out_len = len(self.output)
        if out_len >= 1024:
            out_str = f"{out_len / 1024:.1f}k output"
        else:
            out_str = f"{out_len} output"
        prose_len = len(self.prose)
        prose_str = f", {prose_len}ch prose" if prose_len else ""
        sym = "\u2717" if self.error else "\u2713"
        return f"Turn(#{self.index}: {tc} tool calls, {out_str}{prose_str}, {sym})"


class HistoryMatch:
    """A ranked history search match."""

    __slots__ = (
        "turn",
        "score",
        "field_hits",
        "preview",
        "tool_calls",
        "files_read",
        "files_written",
    )

    def __init__(self, turn, score, field_hits, preview, tool_calls, files_read, files_written):
        self.turn = turn
        self.score = score
        self.field_hits = field_hits
        self.preview = preview
        self.tool_calls = tool_calls
        self.files_read = files_read
        self.files_written = files_written

    def __repr__(self):
        fields = ",".join(self.field_hits) if self.field_hits else "none"
        return f"HistoryMatch(turn={self.turn}, score={self.score:.3f}, fields={fields})"

    def __str__(self):
        return self.__repr__()


class TurnHistory:
    _MAX_TURNS = 2000
    def __init__(self):
        self._turns = []

    def __getitem__(self, key):
        return self._turns[key]

    def __len__(self):
        return len(self._turns)

    def __iter__(self):
        return iter(self._turns)

    def __repr__(self):
        n = len(self._turns)
        files = len(self.files_modified())
        return f"TurnHistory({n} turns, {files} files touched)"

    def user_messages(self):
        """All unique user messages across turns (what the user asked)."""
        seen = set()
        out = []
        for t in self._turns:
            if t.user_message and t.user_message not in seen:
                seen.add(t.user_message)
                out.append(t.user_message)
        return out

    def search(self, query, mode="hybrid", regex=None, limit=10, fields=None, since_turn=None):
        """Search turns using hybrid/literal/regex matching."""
        return _search_history_matches(
            turns=self._turns,
            query=query,
            mode=mode,
            regex=regex,
            limit=limit,
            fields=fields,
            since_turn=since_turn,
        )

    def tool_calls(self, tool=None):
        """All tool calls, optionally filtered by tool name."""
        out = []
        for t in self._turns:
            for tc in t.tool_calls:
                if tool is None or tc.tool.value == tool or tc.tool == tool:
                    out.append(tc)
        return out

    def files_read(self):
        """All unique files read across all turns."""
        s = set()
        for t in self._turns:
            s.update(t.files_read)
        return sorted(s)

    def files_modified(self):
        """All unique files written/edited across all turns."""
        s = set()
        for t in self._turns:
            s.update(t.files_written)
        return sorted(s)

    def errors(self):
        """Turns that had errors."""
        return [t for t in self._turns if t.error]

    def summary(self):
        """Auto-generated brief summary."""
        n = len(self._turns)
        tc = sum(len(t.tool_calls) for t in self._turns)
        errs = len(self.errors())
        fr = self.files_read()
        fm = self.files_modified()
        um = self.user_messages()
        lines = [f"{n} turns, {tc} tool calls, {errs} errors"]
        if um:
            lines.append(f"User asked: {' | '.join(um)}")
        if fr:
            lines.append(f"Files read: {', '.join(fr[:10])}" + (f" (+{len(fr)-10} more)" if len(fr) > 10 else ""))
        if fm:
            lines.append(f"Files modified: {', '.join(fm[:10])}" + (f" (+{len(fm)-10} more)" if len(fm) > 10 else ""))
        return "\n".join(lines)

    def _add_turn(self, json_str):
        """Deserialize a turn from JSON (called from Rust)."""
        data = json.loads(json_str)
        tcs = []
        files_read = []
        files_written = []
        for tc_data in data.get("tool_calls", []):
            tool_name = _TOOL_NAME_MAP.get(tc_data.get("tool", ""), ToolName.OTHER)
            args = tc_data.get("args", {})
            # Extract args dict from serde_json::Value
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except Exception:
                    args = {}
            tc = ToolCall(
                tool=tool_name,
                args=args if isinstance(args, dict) else {},
                result=tc_data.get("result"),
                success=tc_data.get("success", False),
                duration_ms=tc_data.get("duration_ms", 0),
            )
            tcs.append(tc)
            path = tc.args.get("path")
            if path:
                if tool_name in _READ_TOOLS:
                    files_read.append(path)
                if tool_name in _WRITE_TOOLS:
                    files_written.append(path)
        turn = Turn(
            index=data.get("index", len(self._turns)),
            user_message=data.get("user_message", ""),
            prose=data.get("prose", ""),
            code=data.get("code", ""),
            output=data.get("output", ""),
            error=data.get("error"),
            tool_calls=tcs,
            files_read=files_read,
            files_written=files_written,
        )
        self._turns.append(turn)
        if len(self._turns) > self._MAX_TURNS:
            self._turns = self._turns[-self._MAX_TURNS:]

    def _serialize(self):
        """Serialize all turns to a JSON string for passing to sub-agents."""
        turns = []
        for t in self._turns:
            turns.append({
                "index": t.index,
                "user_message": t.user_message,
                "prose": t.prose,
                "code": t.code,
                "output": t.output,
                "error": t.error,
                "tool_calls": [
                    {
                        "tool": tc.tool.value,
                        "args": tc.args,
                        "result": tc.result,
                        "success": tc.success,
                        "duration_ms": tc.duration_ms,
                    }
                    for tc in t.tool_calls
                ],
            })
        return json.dumps(turns)

    def _load(self, data):
        """Load turns from a list of dicts or JSON string (used to inherit parent agent state)."""
        if isinstance(data, str):
            data = json.loads(data)
        # Only load the most recent turns to respect the cap
        if len(data) > self._MAX_TURNS:
            data = data[-self._MAX_TURNS:]
        for t_data in data:
            self._add_turn(json.dumps(t_data))
# ─── Agent Memory ───

class MemEntry:
    __slots__ = ("key", "description", "value", "turn")

    def __init__(self, key, description, value, turn):
        self.key = key
        self.description = description
        self.value = value
        self.turn = turn

    def __repr__(self):
        return f"MemEntry({self.key!r}, turn={self.turn})"


class MemMatch:
    """A ranked memory search match."""

    __slots__ = ("key", "description", "value", "turn", "score", "field_hits")

    def __init__(self, key, description, value, turn, score, field_hits):
        self.key = key
        self.description = description
        self.value = value
        self.turn = turn
        self.score = score
        self.field_hits = field_hits

    def __repr__(self):
        fields = ",".join(self.field_hits) if self.field_hits else "none"
        return f"MemMatch(key={self.key!r}, turn={self.turn}, score={self.score:.3f}, fields={fields})"

    def __str__(self):
        return self.__repr__()


class Mem:
    """Persistent key-value memory for the agent. Values are stringified on store."""

    def __init__(self):
        self._store = {}  # key -> MemEntry
        self._current_turn = 0

    def _set_turn(self, turn):
        """Called by the runtime to track the current turn number."""
        self._current_turn = turn

    def set(self, key, description, value=None):
        """Store a value. The value is stringified via str(). Updates turn to current."""
        self._store[key] = MemEntry(
            key=key,
            description=str(description),
            value=str(value) if value is not None else str(description),
            turn=self._current_turn,
        )
        return _Awaitable()

    def get(self, key):
        """Get the stored value string for a key, or None."""
        entry = self._store.get(key)
        return entry.value if entry else None

    def entry(self, key):
        """Get the full MemEntry for a key, or None."""
        return self._store.get(key)

    def delete(self, key):
        """Remove a key."""
        self._store.pop(key, None)
        return _Awaitable()

    def all(self):
        """List all keys with descriptions and turn numbers."""
        if not self._store:
            return "(empty)"
        lines = []
        for key, e in self._store.items():
            lines.append(f"  [{e.turn}] {key}: {e.description}")
        return "\n".join(lines)

    def search(self, query, mode="hybrid", regex=None, limit=10, keys=None):
        """Search memory entries using hybrid/literal/regex matching."""
        return _search_mem_matches(
            entries=list(self._store.values()),
            query=query,
            mode=mode,
            regex=regex,
            limit=limit,
            keys=keys,
        )

    def since(self, turn):
        """Get all entries set/updated at or after the given turn."""
        return [e for e in self._store.values() if e.turn >= turn]

    def recent(self, n=10):
        """Get entries from the last n turns."""
        cutoff = max(0, self._current_turn - n)
        return self.since(cutoff)

    def __repr__(self):
        return self.all()

    def __len__(self):
        return len(self._store)


    def _serialize(self):
        """Serialize all entries to a JSON string for passing to sub-agents."""
        entries = []
        for key, e in self._store.items():
            entries.append({
                "key": e.key,
                "description": e.description,
                "value": e.value,
                "turn": e.turn,
            })
        return json.dumps(entries)

    def _load(self, data):
        """Load entries from a list of dicts or JSON string (used to inherit parent agent state)."""
        if isinstance(data, str):
            data = json.loads(data)
        entries = data
        for e in entries:
            self._store[e["key"]] = MemEntry(
                key=e["key"],
                description=e["description"],
                value=e["value"],
                turn=e["turn"],
            )
# _rust_bridge is injected by the Rust runtime before any functions are called.
# It provides:
#   send_message(json_str) -> None
#   invoke_tool(py, call_id, name, args_json) -> str
_rust_bridge = None

# --- Persistent REPL namespace ---
_ns = {}
_tools_initialized = False
_headless = False

# --- Tool call resolution ---
_pending_calls = {}  # id -> asyncio.Future
_loop = None


class _Awaitable:
    """Immediately-resolved awaitable that optionally carries a return value."""
    def __init__(self, value=None):
        self._value = value

    def __await__(self):
        return self._value
        yield  # makes this a generator; unreachable but required

    def __repr__(self):
        return repr(self._value)

    def __str__(self):
        return str(self._value) if self._value is not None else ""


def _send(msg):
    """Send a message to Rust via the bridge."""
    _rust_bridge.send_message(json.dumps(msg))


_MAX_OUTPUT_LEN = 20_000
_MAX_STDOUT_LEN = 20_000
_done_requested = False

def _truncate_output(text, limit=_MAX_STDOUT_LEN):
    """Head+tail truncation for stdout captured during exec."""
    if len(text) <= limit:
        return text
    half = limit // 2
    omitted = len(text) - limit
    return f"{text[:half]}\n\n... ({omitted:,} chars omitted) ...\n\n{text[-half:]}"

def _format_value(value):
    """Format a value as the REPL would: strings as-is, others via repr()."""
    if isinstance(value, str):
        text = value.strip()
    elif value is None:
        return None  # skip, like REPL
    else:
        text = repr(value)
    if not text:
        return None
    if len(text) > _MAX_OUTPUT_LEN:
        text = text[:_MAX_OUTPUT_LEN] + f"\n... [truncated, {len(text)} chars total]"
    return text

def _done(value=""):
    """End the turn with a final result."""
    global _done_requested
    text = _format_value(value)
    if text is None:
        text = ""
    _send({"type": "message", "text": text, "kind": "final"})
    _done_requested = True
    return _Awaitable()




async def _ask(question, options=None):
    """Ask the user a question. Blocks until they respond."""
    if _headless:
        raise RuntimeError("ask() is unavailable in headless mode")
    payload = json.dumps({"question": str(question), "options": options})
    loop = asyncio.get_event_loop()
    result = await loop.run_in_executor(None, _rust_bridge.ask_user, payload)
    # Print the answer so it appears in execution output, ensuring the agent
    # continues to the next LLM iteration with the user's response in context.
    print(f"[User response: {result}]")
    return result


class ShellHandle:
    """Handle to a running shell process.

    Returned by shell(command=...). Provides:
      .result(timeout=None) — wait for exit, return output
      .write(text)         — send stdin input
      .output()            — read accumulated output (non-blocking)
      .kill()              — kill the process group
    """
    def __init__(self, id):
        self.id = id

    async def result(self, timeout=None):
        """Wait for the process to exit and return its full output."""
        return await _call("shell_result", {"id": self.id, "timeout": timeout})

    async def write(self, text):
        """Send input to the process's stdin."""
        return await _call("shell_write", {"id": self.id, "input": text})

    async def output(self):
        """Read accumulated output so far (non-blocking)."""
        return await _call("shell_output", {"id": self.id})

    async def kill(self):
        """Kill the process group."""
        return await _call("shell_kill", {"id": self.id})

    def __repr__(self):
        return f"ShellHandle(id='{self.id}')"

    def __str__(self):
        return f"ShellHandle({self.id})"



class AgentHandle:
    """Handle to a running sub-agent.

    Returned by agent_call(prompt=..., intelligence=...). Provides:
      .result(timeout=None) -- wait for completion, return result
      .output()             -- read accumulated output (non-blocking)
      .kill()               -- cancel the sub-agent
    """
    def __init__(self, id, schema_cls=None):
        self.id = id
        self._schema_cls = schema_cls

    async def result(self, timeout=None):
        """Wait for the sub-agent to finish and return its result.

        If a schema was provided, returns a hydrated Pydantic model instance.
        Otherwise returns a dict with "result" and "context" keys.
        """
        raw = await _call("agent_result", {"id": self.id, "timeout": timeout})
        if self._schema_cls is not None and hasattr(self._schema_cls, "model_validate"):
            val = raw.get("result", "") if isinstance(raw, dict) else raw
            if isinstance(val, str):
                try:
                    val = json.loads(val)
                except (json.JSONDecodeError, TypeError):
                    pass
            if isinstance(val, dict):
                return self._schema_cls.model_validate(val)
        return raw

    async def output(self):
        """Read accumulated output so far (non-blocking)."""
        return await _call("agent_output", {"id": self.id})

    async def kill(self):
        """Cancel the sub-agent."""
        return await _call("agent_kill", {"id": self.id})

    def __repr__(self):
        return f"AgentHandle(id='{self.id}')"

    def __str__(self):
        return f"AgentHandle({self.id})"

class Task:
    """A task in the task management system.

    Returned by create_task(), get_task(), update_task(), and items in tasks().
    Provides convenience methods that call the underlying tools.

    repr: Task(a1b2 ~ 'Fix auth bug' high)
    str:  [~ in_progress] Fix auth bug  (a1b2, high)
          description text here
          blocked_by: c3d4, e5f6
    """

    _STATUS_SYMBOLS = {
        "pending": "\u25cb",      # ○
        "in_progress": "~",
        "completed": "\u2713",    # ✓
        "cancelled": "\u2717",    # ✗
    }

    def __init__(self, data):
        self.id = data.get("id", "")
        self.subject = data.get("subject", "")
        self.description = data.get("description", "")
        self.status = data.get("status", "pending")
        self.priority = data.get("priority", "medium")
        self.active_form = data.get("active_form", "")
        self.owner = data.get("owner", "")
        self.blocks = data.get("blocks", [])
        self.blocked_by = data.get("blocked_by", [])
        self.metadata = data.get("metadata", {})

    def _sym(self):
        return self._STATUS_SYMBOLS.get(self.status, "?")

    def __repr__(self):
        return f"Task({self.id} {self._sym()} '{self.subject}' {self.priority})"

    def __str__(self):
        lines = [f"[{self._sym()} {self.status}] {self.subject}  ({self.id}, {self.priority})"]
        if self.description:
            lines.append(f"  {self.description}")
        if self.blocked_by:
            lines.append(f"  blocked_by: {', '.join(self.blocked_by)}")
        if self.blocks:
            lines.append(f"  blocks: {', '.join(self.blocks)}")
        return "\n".join(lines)

    def _refresh(self, other):
        """Update self from another Task instance (returned by server)."""
        self.id = other.id
        self.subject = other.subject
        self.description = other.description
        self.status = other.status
        self.priority = other.priority
        self.active_form = other.active_form
        self.owner = other.owner
        self.blocks = other.blocks
        self.blocked_by = other.blocked_by
        self.metadata = other.metadata
        return self

    async def start(self):
        """Claim this task and set status to in_progress."""
        result = await _call("claim_task", {"id": self.id, "owner": _ns.get("__agent_id__", "")})
        return self._refresh(result)

    async def done(self):
        """Set status to completed."""
        result = await _call("update_task", {"id": self.id, "status": "completed"})
        return self._refresh(result)

    async def cancel(self):
        """Set status to cancelled."""
        result = await _call("update_task", {"id": self.id, "status": "cancelled"})
        return self._refresh(result)

    async def delete(self):
        """Permanently remove this task."""
        await _call("delete_task", {"id": self.id})

    async def block(self, *ids):
        """Mark task IDs that this task blocks."""
        result = await _call("update_task", {"id": self.id, "add_blocks": list(ids)})
        return self._refresh(result)

    async def wait_on(self, *ids):
        """Mark task IDs that block this task."""
        result = await _call("update_task", {"id": self.id, "add_blocked_by": list(ids)})
        return self._refresh(result)

    async def update(self, **kw):
        """General update -- pass any updatable fields as keyword args."""
        kw["id"] = self.id
        result = await _call("update_task", kw)
        return self._refresh(result)


class SkillSummary:
    """Summary of a skill (name + description only).

    repr: Skill('deploy')
    str:  deploy -- Deploy to production (2 files)
    """
    def __init__(self, data):
        self.name = data.get("name", "")
        self.description = data.get("description", "")
        self.file_count = data.get("file_count", 0)
        self.score = float(data.get("score", 0.0))

    def __repr__(self):
        return f"Skill('{self.name}')"

    def __str__(self):
        parts = [self.name]
        if self.description:
            parts.append(f"-- {self.description}")
        if self.file_count > 0:
            parts.append(f"({self.file_count} files)")
        return " ".join(parts)

    async def load(self):
        return await _call("load_skill", {"name": self.name})


class Skill:
    """A loaded skill with full instructions.

    repr: Skill('deploy', 2 files)
    str:  deploy -- Deploy to production (2 files)
    Access .instructions for the full markdown body.
    """
    def __init__(self, data):
        self.name = data.get("name", "")
        self.description = data.get("description", "")
        self.instructions = data.get("instructions", "")
        self.files = data.get("files", [])

    def __repr__(self):
        if self.files:
            return f"Skill('{self.name}', {len(self.files)} files)"
        return f"Skill('{self.name}')"

    def __str__(self):
        parts = [self.name]
        if self.description:
            parts.append(f"-- {self.description}")
        if self.files:
            parts.append(f"({len(self.files)} files)")
        return " ".join(parts)

    async def read_file(self, path):
        return await _call("read_skill_file", {"skill_name": self.name, "path": path})


class EditResult:
    """Structured result returned by edit_file().

    Fields:
      .summary  -- short status line
      .diff     -- unified diff text (may be empty)
    """

    def __init__(self, data):
        self.summary = data.get("summary", "")
        self.diff = data.get("diff", "")

    def __repr__(self):
        return self.summary

    def __str__(self):
        return self.summary


class PathEntry:
    """Filesystem entry returned by glob()/ls()."""

    def __init__(self, data):
        self.path = data.get("path", "")
        self.kind = data.get("kind", "other")
        self.size_bytes = data.get("size_bytes", 0)
        self.lines = data.get("lines", None)
        self.modified_at = data.get("modified_at", "")

    def as_dict(self):
        return {
            "path": self.path,
            "kind": self.kind,
            "size_bytes": self.size_bytes,
            "lines": self.lines,
            "modified_at": self.modified_at,
        }

    def __repr__(self):
        lines_part = f", lines={self.lines}" if self.lines is not None else ""
        return (
            f"PathEntry(path={self.path!r}, kind={self.kind!r}, size_bytes={self.size_bytes}"
            f"{lines_part}, modified_at={self.modified_at!r})"
        )


class PathEntries(list):
    """List-like wrapper for path entry results with truncation metadata."""

    def __init__(self, data):
        items = [PathEntry(item) for item in data.get("items", [])]
        super().__init__(items)
        self.truncated = data.get("truncated")

    @property
    def shown(self):
        if isinstance(self.truncated, dict):
            return self.truncated.get("shown")
        return len(self)

    @property
    def total(self):
        if isinstance(self.truncated, dict):
            return self.truncated.get("total")
        return len(self)

    @property
    def omitted(self):
        if isinstance(self.truncated, dict):
            return self.truncated.get("omitted")
        return 0

    def __repr__(self):
        if isinstance(self.truncated, dict):
            return (
                f"PathEntries({len(self)} shown, total={self.total}, omitted={self.omitted})"
            )
        return f"PathEntries({len(self)})"


class ToolError(Exception):
    """Raised when a tool call fails.

    Extends Exception so it stops execution at the failing statement.
    Use asyncio.gather(..., return_exceptions=True) to collect failures
    without stopping.
    """
    def __init__(self, name, error):
        self.name = name
        self.error = error
        super().__init__(f"{name}: {error}")

    def __repr__(self):
        return f"ToolError({self.name!r}, {self.error!r})"

    def __bool__(self):
        return False


async def _call(name, params):
    """Call a tool by name. Returns parsed JSON result or ToolError on failure."""
    call_id = str(uuid.uuid4())
    args_json = json.dumps(params)

    # Use run_in_executor to offload the blocking Rust call to a thread pool.
    # This keeps the asyncio loop responsive for concurrent tool calls.
    loop = asyncio.get_event_loop()
    result_json = await loop.run_in_executor(
        None, _rust_bridge.invoke_tool, call_id, name, args_json
    )

    result = json.loads(result_json)
    if result["success"]:
        value = json.loads(result["result"]) if result["result"] else None
        # Wrap shell handles automatically
        if isinstance(value, dict) and value.get("__handle__") == "shell":
            return ShellHandle(value["id"])
        # Wrap agent handles automatically
        if isinstance(value, dict) and value.get("__handle__") == "agent":
            return AgentHandle(value["id"])
        # Wrap typed objects
        if isinstance(value, dict) and "__type__" in value:
            t = value["__type__"]
            if t == "task":
                return Task(value)
            if t == "task_list":
                return [Task(item) for item in value.get("items", [])]
            if t == "skill":
                return Skill(value)
            if t == "skill_summary":
                return SkillSummary(value)
            if t == "skill_list":
                return [SkillSummary(item) for item in value.get("items", [])]
            if t == "edit_result":
                return EditResult(value)
            if t == "path_entries":
                return PathEntries(value)
        return value
    else:
        error = json.loads(result["result"]) if result["result"] else "Tool call failed"
        raise ToolError(name, error)


_TYPE_MAP = {
    "str": str, "string": str,
    "int": int, "integer": int,
    "float": float, "number": float,
    "bool": bool, "boolean": bool,
    "list": list, "dict": dict,
    "any": typing.Any, "None": type(None),
}

_tool_defs = []
_capabilities = {"memory": True, "history": True}
_FIND_DEFAULT_LIMIT = 10
_FIND_MAX_LIMIT = 100


def _coerce_limit(limit):
    try:
        n = int(limit)
    except (TypeError, ValueError):
        n = _FIND_DEFAULT_LIMIT
    return max(1, min(_FIND_MAX_LIMIT, n))


def _coerce_find_mode(mode):
    m = str(mode or "hybrid").strip().lower()
    return m if m in {"hybrid", "regex", "literal"} else "hybrid"


def _normalize_list_arg(value):
    if value is None:
        return None
    if isinstance(value, str):
        parts = [p.strip() for p in value.split(",")]
        return [p for p in parts if p]
    if isinstance(value, (list, tuple, set)):
        return [str(v).strip() for v in value if str(v).strip()]
    return [str(value).strip()]


def _compile_regex(pattern):
    if pattern is None:
        return None
    raw = str(pattern)
    if not raw:
        return None
    try:
        return re.compile(raw, re.IGNORECASE)
    except re.error:
        return re.compile(re.escape(raw), re.IGNORECASE)


def _tokenize(text):
    if text is None:
        return []
    return [t for t in re.split(r"[^a-zA-Z0-9_]+", str(text).lower()) if t]


def _truncate_preview(text, limit=220):
    s = str(text or "").strip().replace("\n", " ")
    if len(s) <= limit:
        return s
    return s[: max(0, limit - 3)] + "..."


def _document_haystack(doc, field_names):
    return "\n".join(str(doc.get(field, "") or "") for field in field_names)


def _bm25_scores(query_tokens, docs, field_weights, k1=1.5, b=0.75):
    n_docs = len(docs)
    if n_docs == 0:
        return []

    doc_tfs = []
    doc_lens = []
    doc_freq = defaultdict(int)

    for doc in docs:
        tf = defaultdict(float)
        dlen = 0.0
        for field, weight in field_weights.items():
            if weight <= 0:
                continue
            tokens = _tokenize(doc.get(field, ""))
            if not tokens:
                continue
            counts = Counter(tokens)
            for tok, count in counts.items():
                tf[tok] += float(count) * float(weight)
            dlen += float(len(tokens)) * float(weight)
        doc_tfs.append(tf)
        doc_lens.append(dlen)
        for tok in tf.keys():
            doc_freq[tok] += 1

    avgdl = sum(doc_lens) / float(n_docs) if n_docs else 1.0
    if avgdl <= 0:
        avgdl = 1.0

    qtf = Counter(query_tokens)
    scores = [0.0 for _ in range(n_docs)]

    for i, tf in enumerate(doc_tfs):
        dl = doc_lens[i]
        norm = 1.0 - b + b * (dl / avgdl)
        for tok, qcount in qtf.items():
            freq = tf.get(tok, 0.0)
            if freq <= 0:
                continue
            df = doc_freq.get(tok, 0)
            idf = math.log(1.0 + ((n_docs - df + 0.5) / (df + 0.5)))
            denom = freq + k1 * norm
            if denom <= 0:
                continue
            term = idf * ((freq * (k1 + 1.0)) / denom)
            scores[i] += term * (1.0 + math.log(float(qcount)))

    return scores


def _compute_field_hits(field_texts, query, mode, regex_filter):
    mode = _coerce_find_mode(mode)
    query = str(query or "")
    query_lower = query.lower()
    query_tokens = set(_tokenize(query))
    hits = []
    for field, value in field_texts.items():
        text = str(value or "")
        if not text:
            continue
        text_lower = text.lower()
        hit = False
        if mode == "regex":
            hit = bool(regex_filter and regex_filter.search(text))
        elif mode == "literal":
            hit = bool(query and query_lower in text_lower)
            if hit and regex_filter is not None:
                hit = regex_filter.search(text) is not None
        else:
            if query_tokens:
                field_tokens = set(_tokenize(text))
                hit = any(tok in field_tokens for tok in query_tokens)
            elif query:
                hit = query_lower in text_lower
            else:
                hit = True
            if hit and regex_filter is not None:
                hit = regex_filter.search(text) is not None
        if hit:
            hits.append(field)
    return hits


def _rank_documents(docs, query, mode, regex, field_weights, field_names):
    mode = _coerce_find_mode(mode)
    query = str(query or "")
    query_lower = query.lower()
    query_tokens = _tokenize(query)

    regex_filter = None
    if mode == "regex":
        regex_filter = _compile_regex(regex if regex is not None else query)
    elif regex is not None:
        regex_filter = _compile_regex(regex)

    scores = [0.0] * len(docs)
    if mode == "hybrid" and query_tokens:
        scores = _bm25_scores(query_tokens, docs, field_weights)

    ranked_indices = list(range(len(docs)))
    if mode == "hybrid":
        ranked_indices.sort(key=lambda i: (-scores[i], i))

    out = []
    for idx in ranked_indices:
        haystack = _document_haystack(docs[idx], field_names)
        haystack_lower = haystack.lower()

        include = True
        if mode == "regex":
            include = bool(regex_filter and regex_filter.search(haystack))
        elif mode == "literal":
            include = bool(query and query_lower in haystack_lower)
            if include and regex_filter is not None:
                include = regex_filter.search(haystack) is not None
        else:
            if query:
                if query_tokens:
                    include = scores[idx] > 0 or query_lower in haystack_lower
                else:
                    include = query_lower in haystack_lower
            if include and regex_filter is not None:
                include = regex_filter.search(haystack) is not None

        if include:
            out.append((idx, scores[idx], regex_filter))
    return out


def _tool_signature(tool):
    params = tool.get("params", [])
    parts = []
    for p in params:
        if isinstance(p, dict):
            ty = p.get("type", "any")
            name = p.get("name", "arg")
            part = f"{name}: {ty}"
            if not p.get("required", True):
                part += " = None"
            parts.append(part)
        else:
            parts.append(str(p))
    ret = tool.get("returns", "any") or "any"
    return f"{tool.get('name', 'tool')}({', '.join(parts)}) -> {ret}"


def _tool_oneliner(desc):
    text = (desc or "").strip()
    if not text:
        return ""
    return text.splitlines()[0].strip()


def _default_example(tool):
    params = []
    for p in tool.get("params", []):
        if not isinstance(p, dict):
            continue
        pname = p.get("name", "arg")
        ptype = (p.get("type", "any") or "any").lower()
        placeholder = "..."
        if ptype in ("str", "string"):
            placeholder = f'"{pname}"'
        elif ptype in ("int", "integer"):
            placeholder = "1"
        elif ptype in ("float", "number"):
            placeholder = "1.0"
        elif ptype in ("bool", "boolean"):
            placeholder = "True"
        elif ptype == "list":
            placeholder = "[]"
        elif ptype == "dict":
            placeholder = "{}"
        params.append(f"{pname}={placeholder}")
    return f"{tool.get('name', 'tool')}({', '.join(params)})"


class ToolInfo:
    """Structured tool metadata returned by list_tools/search_tools."""

    def __init__(self, data):
        self.name = data.get("name", "")
        self.description = data.get("description", "")
        self.oneliner = _tool_oneliner(self.description)
        self.params = data.get("params", [])
        self.returns = data.get("returns", "any")
        self.examples = data.get("examples", []) or []
        if not self.examples:
            self.examples = [_default_example(data)]
        self.signature = _tool_signature(data)
        self.score = float(data.get("score", 0.0))
        self.hidden = bool(data.get("hidden", False))
        self.inject_into_prompt = bool(data.get("inject_into_prompt", False))

    def __repr__(self):
        if self.oneliner:
            return f"{self.name} - {self.oneliner}"
        return self.name

    def __str__(self):
        return self.__repr__()


def _select_tools(query=None, injected_only=None):
    selected = []
    for t in _tool_defs:
        if injected_only is True and not t.get("inject_into_prompt", False):
            continue
        if injected_only is False and t.get("inject_into_prompt", False):
            continue
        selected.append(ToolInfo(t))
    selected.sort(key=lambda ti: ti.name)
    if query is not None and str(query).strip():
        q = str(query).lower()
        selected = [t for t in selected if q in t.name.lower() or q in t.description.lower()]
    return selected


def _print_tool_index(items):
    if not items:
        print("No tools matched.")
        return
    print("Available tools:")
    for t in items:
        desc = f" - {t.oneliner}" if t.oneliner else ""
        score = f" [{t.score:.3f}]" if getattr(t, "score", 0.0) > 0 else ""
        print(f"  {t.name}{score}{desc}")


def _list_tools(query=None, injected_only=None, **_ignored):
    """Return tool metadata objects. Also prints a compact index."""
    items = _select_tools(
        query=query,
        injected_only=injected_only,
    )
    _print_tool_index(items)
    return items


def _search_tools(query, mode="hybrid", regex=None, limit=10, injected_only=None, **_ignored):
    """Search tools using hybrid/literal/regex matching across all tools."""
    limit = _coerce_limit(limit)
    candidates = []
    for t in _tool_defs:
        if injected_only is True and not t.get("inject_into_prompt", False):
            continue
        if injected_only is False and t.get("inject_into_prompt", False):
            continue
        candidates.append(t)

    docs = [
        {
            "name": t.get("name", ""),
            "description": t.get("description", ""),
            "examples": "\n".join(str(x) for x in (t.get("examples", []) or [])),
        }
        for t in candidates
    ]
    field_weights = {"name": 4.0, "description": 2.0, "examples": 1.0}
    field_names = list(field_weights.keys())
    ranked = _rank_documents(
        docs=docs,
        query=query,
        mode=mode,
        regex=regex,
        field_weights=field_weights,
        field_names=field_names,
    )

    items = []
    for idx, score, _ in ranked[:limit]:
        tool_data = dict(candidates[idx])
        tool_data["score"] = float(score)
        items.append(ToolInfo(tool_data))
    _print_tool_index(items)
    return items


def _print_skill_index(items):
    if not items:
        print("No skills matched.")
        return
    print("Skill matches:")
    for s in items:
        score = f" [{s.score:.3f}]" if getattr(s, "score", 0.0) > 0 else ""
        desc = f" - {s.description}" if s.description else ""
        print(f"  {s.name}{score}{desc}")


async def _search_skills(query, mode="hybrid", regex=None, limit=10):
    """Search skill names/descriptions using hybrid/literal/regex matching."""
    limit = _coerce_limit(limit)
    if not any(t.get("name") == "skills" for t in _tool_defs):
        return []

    raw_items = await _call("skills", {})
    summaries = []
    for item in raw_items or []:
        if isinstance(item, SkillSummary):
            summaries.append(item)
        elif isinstance(item, dict):
            summaries.append(SkillSummary(item))

    docs = [
        {
            "name": s.name,
            "description": s.description,
        }
        for s in summaries
    ]
    field_weights = {"name": 4.0, "description": 2.0}
    field_names = list(field_weights.keys())
    ranked = _rank_documents(
        docs=docs,
        query=query,
        mode=mode,
        regex=regex,
        field_weights=field_weights,
        field_names=field_names,
    )

    items = []
    for idx, score, _ in ranked[:limit]:
        s = summaries[idx]
        items.append(
            SkillSummary(
                {
                    "name": s.name,
                    "description": s.description,
                    "file_count": s.file_count,
                    "score": float(score),
                }
            )
        )
    _print_skill_index(items)
    return items


def _normalize_history_fields(fields):
    if fields is None:
        return ["user_message", "code", "prose", "output", "tool_calls"]
    mapping = {
        "user": "user_message",
        "user_message": "user_message",
        "code": "code",
        "prose": "prose",
        "output": "output",
        "tool_calls": "tool_calls",
    }
    selected = []
    for f in _normalize_list_arg(fields) or []:
        key = mapping.get(str(f).strip().lower())
        if key and key not in selected:
            selected.append(key)
    return selected or ["user_message", "code", "prose", "output", "tool_calls"]


def _search_history_matches(turns, query, mode="hybrid", regex=None, limit=10, fields=None, since_turn=None):
    limit = _coerce_limit(limit)
    selected_fields = _normalize_history_fields(fields)
    filtered_turns = []
    for t in turns:
        if since_turn is not None:
            try:
                if int(t.index) < int(since_turn):
                    continue
            except (TypeError, ValueError):
                pass
        filtered_turns.append(t)

    docs = []
    for t in filtered_turns:
        tool_text = " ".join(
            f"{tc.tool.value} {json.dumps(tc.args, sort_keys=True)}"
            for tc in t.tool_calls
        )
        docs.append(
            {
                "user_message": t.user_message,
                "code": t.code,
                "prose": t.prose,
                "output": t.output,
                "tool_calls": tool_text,
            }
        )

    base_weights = {
        "user_message": 3.5,
        "code": 2.8,
        "prose": 1.5,
        "output": 1.0,
        "tool_calls": 1.2,
    }
    field_weights = {k: v for k, v in base_weights.items() if k in selected_fields}
    field_names = list(field_weights.keys())
    ranked = _rank_documents(
        docs=docs,
        query=query,
        mode=mode,
        regex=regex,
        field_weights=field_weights,
        field_names=field_names,
    )

    out = []
    for idx, score, regex_filter in ranked[:limit]:
        t = filtered_turns[idx]
        field_texts = {k: docs[idx].get(k, "") for k in field_names}
        hits = _compute_field_hits(field_texts, query, mode, regex_filter)
        preview_source = next(
            (field_texts.get(h, "") for h in hits if field_texts.get(h, "")),
            next((field_texts.get(k, "") for k in field_names if field_texts.get(k, "")), ""),
        )
        out.append(
            HistoryMatch(
                turn=t.index,
                score=float(score),
                field_hits=hits,
                preview=_truncate_preview(preview_source),
                tool_calls=[tc.tool.value for tc in t.tool_calls],
                files_read=list(t.files_read),
                files_written=list(t.files_written),
            )
        )
    return out


def _print_history_index(items):
    if not items:
        print("No history matches.")
        return
    print("History matches:")
    for m in items:
        fields = ",".join(m.field_hits) if m.field_hits else "none"
        score = f"{m.score:.3f}"
        print(f"  turn {m.turn} [{score}] ({fields}) {m.preview}")


def _search_history(query, mode="hybrid", regex=None, limit=10, fields=None, since_turn=None):
    history = _ns.get("_history")
    if history is None:
        return []
    items = history.search(
        query=query,
        mode=mode,
        regex=regex,
        limit=limit,
        fields=fields,
        since_turn=since_turn,
    )
    _print_history_index(items)
    return items


def _search_mem_matches(entries, query, mode="hybrid", regex=None, limit=10, keys=None):
    limit = _coerce_limit(limit)
    key_filter = set(_normalize_list_arg(keys) or [])
    filtered_entries = [e for e in entries if not key_filter or e.key in key_filter]
    docs = [
        {
            "key": e.key,
            "description": e.description,
            "value": e.value,
        }
        for e in filtered_entries
    ]
    field_weights = {"key": 4.0, "description": 2.0, "value": 1.0}
    field_names = list(field_weights.keys())
    ranked = _rank_documents(
        docs=docs,
        query=query,
        mode=mode,
        regex=regex,
        field_weights=field_weights,
        field_names=field_names,
    )

    out = []
    for idx, score, regex_filter in ranked[:limit]:
        e = filtered_entries[idx]
        field_texts = docs[idx]
        hits = _compute_field_hits(field_texts, query, mode, regex_filter)
        out.append(
            MemMatch(
                key=e.key,
                description=e.description,
                value=e.value,
                turn=e.turn,
                score=float(score),
                field_hits=hits,
            )
        )
    return out


def _print_mem_index(items):
    if not items:
        print("No memory matches.")
        return
    print("Memory matches:")
    for m in items:
        fields = ",".join(m.field_hits) if m.field_hits else "none"
        print(
            f"  {m.key} [turn {m.turn}] [{m.score:.3f}] ({fields}) "
            f"{_truncate_preview(m.description, 120)}"
        )


def _search_mem(query, mode="hybrid", regex=None, limit=10, keys=None):
    mem = _ns.get("_mem")
    if mem is None:
        return []
    items = mem.search(query=query, mode=mode, regex=regex, limit=limit, keys=keys)
    _print_mem_index(items)
    return items


def _reset_repl():
    """Reset the REPL namespace and re-register tools."""
    global _tools_initialized
    # Preserve the stored tool definitions
    saved_defs = json.dumps(_tool_defs)
    saved_agent_id = _ns.get("__agent_id__", "")
    saved_headless = _headless
    saved_capabilities = dict(_capabilities)
    _ns.clear()
    _tools_initialized = False
    _register_tools(saved_defs, saved_agent_id, saved_headless, saved_capabilities)
    print("REPL reset: namespace cleared, tools re-registered.")
    return _Awaitable("REPL reset complete")


def _register_tools(tools_json, agent_id="", headless=False, capabilities=None):
    """Register tool wrappers from JSON tool definitions."""
    global _tools_initialized, _tool_defs, _headless, _async_method_names, _capabilities
    if _tools_initialized:
        return
    _headless = bool(headless)
    caps = {"memory": True, "history": True}
    if isinstance(capabilities, str):
        try:
            capabilities = json.loads(capabilities)
        except Exception:
            capabilities = None
    if isinstance(capabilities, dict):
        if "memory" in capabilities:
            caps["memory"] = bool(capabilities.get("memory"))
        if "history" in capabilities:
            caps["history"] = bool(capabilities.get("history"))
    _capabilities = caps
    _ns["__agent_id__"] = agent_id
    _ns["__capabilities__"] = dict(caps)
    _tool_defs = json.loads(tools_json)
    tools_mod = types.ModuleType("tools")
    exported_tools = {}
    exported_names = []

    for tool in _tool_defs:
        name = tool["name"]
        desc = tool.get("description", "")
        raw_params = tool.get("params", [])
        returns = tool.get("returns", "any")

        # Normalize params
        param_info = []
        for p in raw_params:
            if isinstance(p, str):
                param_info.append({"name": p, "type": "any", "required": True, "description": ""})
            else:
                param_info.append(p)

        def make_fn(n, d, pinfo, ret):
            pnames = [p["name"] for p in pinfo]

            async def fn(*args, **kw):
                params = {}
                for i, arg in enumerate(args):
                    if isinstance(arg, dict):
                        params.update(arg)
                    elif i < len(pnames):
                        params[pnames[i]] = arg
                params.update(kw)
                return await _call(n, params)

            fn.__name__ = n
            fn.__qualname__ = n

            # Build docstring with param types
            sig_parts = []
            doc_parts = [d, ""] if d else []
            for p in pinfo:
                ty = p.get("type", "any")
                req = p.get("required", True)
                part = f"{p['name']}: {ty}"
                if not req:
                    part += " = None"
                sig_parts.append(part)
                pdesc = p.get("description", "")
                if pdesc:
                    doc_parts.append(f"  {p['name']}: {pdesc}")
            if any(p.get("description") for p in pinfo):
                doc_parts.insert(1, "Args:")
            if ret and ret != "any":
                doc_parts.append(f"Returns: {ret}")
            fn.__doc__ = "\n".join(doc_parts) if doc_parts else d

            # Set inspect.Signature for help()
            params_list = []
            for p in pinfo:
                annotation = _TYPE_MAP.get(p.get("type", "any"), typing.Any)
                default = inspect.Parameter.empty if p.get("required", True) else None
                params_list.append(inspect.Parameter(
                    p["name"],
                    inspect.Parameter.POSITIONAL_OR_KEYWORD,
                    annotation=annotation,
                    default=default,
                ))
            ret_annotation = _TYPE_MAP.get(ret, typing.Any)
            fn.__signature__ = inspect.Signature(params_list, return_annotation=ret_annotation)
            return fn

        # Hidden tools are callable via _call() but not exposed directly.
        if not tool.get("hidden", False):
            if name == "agent_call":
                # Wrapped specially below to support schema + parent state transfer.
                continue
            fn = make_fn(name, desc, param_info, returns)
            setattr(tools_mod, name, fn)
            exported_tools[name] = fn
            exported_names.append(name)

    # Override agent_call wrapper: schema hydration + parent memory/history transfer.
    async def _agent_call(prompt, intelligence, schema=None, **kw):
        """Spawn a sub-agent to perform a task. Returns an AgentHandle.

        Args:
            prompt: The task description for the sub-agent.
            intelligence: "low", "medium", or "high".
            schema: Optional Pydantic BaseModel class. If provided, calling
                handle.result() will return a hydrated model instance.

        Returns:
            AgentHandle with .result(), .output(), .kill() methods.
        """
        params = {"prompt": prompt, "intelligence": str(intelligence)}
        params.update(kw)

        # Pass parent _mem and _history to sub-agent (serialized as JSON)
        _hist = _ns.get("_history")
        _memo = _ns.get("_mem")
        if _hist is not None and len(_hist) > 0:
            params["_parent_history"] = _hist._serialize()
        if _memo is not None and len(_memo) > 0:
            params["_parent_mem"] = _memo._serialize()

        # If schema is a Pydantic model class, extract JSON schema and stash the class
        _schema_cls = None
        if schema is not None:
            if hasattr(schema, "model_json_schema"):
                _schema_cls = schema
                params["schema"] = json.dumps(schema.model_json_schema())
            elif isinstance(schema, str):
                params["schema"] = schema
            elif isinstance(schema, dict):
                params["schema"] = json.dumps(schema)

        handle = await _call("agent_call", params)

        # _call returns an AgentHandle via __handle__ detection; attach schema_cls
        if isinstance(handle, AgentHandle) and _schema_cls is not None:
            handle._schema_cls = _schema_cls
        return handle

    _agent_call.__name__ = "agent_call"
    _agent_call.__qualname__ = "agent_call"
    has_visible_agent_call = any(
        t.get("name") == "agent_call" and not t.get("hidden", False) for t in _tool_defs
    )
    if has_visible_agent_call:
        setattr(tools_mod, "agent_call", _agent_call)
        exported_tools["agent_call"] = _agent_call
        exported_names.append("agent_call")
    tools_mod.__all__ = sorted(exported_names)

    has_plan_mode_tools = any(
        t.get("name") in {"enter_plan_mode", "exit_plan_mode"} for t in _tool_defs
    )
    if has_plan_mode_tools:
        # Plan mode wrappers — call Rust tools + orchestrate approval flow
        async def _enter_plan_mode():
            """Enter plan mode. Returns the plan file path."""
            result = await _call("enter_plan_mode", {})
            plan_file = result.get("plan_file", "")
            print(f"[Plan mode — write your plan to: {plan_file}]")
            return plan_file

        async def _exit_plan_mode():
            """Exit plan mode. Interactive sessions ask for approval; headless proceeds autonomously."""
            result = await _call("exit_plan_mode", {})
            plan = result.get("plan_content", "")
            if _headless:
                if plan:
                    print("[Plan mode exited in headless mode — continue autonomously.]")
                    return "Plan finalized in headless mode. Continue executing autonomously."
                return "Plan file is empty. Continue planning autonomously."
            preview = plan[:2000] + ("..." if len(plan) > 2000 else "")
            response = await _ask(
                f"Plan ready for review:\n\n{preview}\n\nHow would you like to proceed?",
                ["Execute plan", "Edit plan", "Reject"]
            )
            if response.startswith("1."):
                _done("Plan approved — executing.")
            return response

        _enter_plan_mode.__name__ = "enter_plan_mode"
        _enter_plan_mode.__qualname__ = "enter_plan_mode"
        _exit_plan_mode.__name__ = "exit_plan_mode"
        _exit_plan_mode.__qualname__ = "exit_plan_mode"

    history_enabled = bool(_capabilities.get("history", True))
    memory_enabled = bool(_capabilities.get("memory", True))
    if history_enabled:
        _ns["_history"] = TurnHistory()
    if memory_enabled:
        _ns["_mem"] = Mem()
    bindings = {
        "json": json, "print": print, "done": _done,
        "asyncio": asyncio, "list_tools": _list_tools, "search_tools": _search_tools,
        "search_skills": _search_skills,
        "reset_repl": _reset_repl,
        "Task": Task, "Skill": Skill, "SkillSummary": SkillSummary, "ToolError": ToolError,
        "Turn": Turn, "ToolCall": ToolCall, "ToolName": ToolName, "Intelligence": Intelligence,
        "ToolInfo": ToolInfo,
        "tools": tools_mod,
    }
    if history_enabled:
        bindings["search_history"] = _search_history
        bindings["TurnHistory"] = TurnHistory
        bindings["HistoryMatch"] = HistoryMatch
    if memory_enabled:
        bindings["search_mem"] = _search_mem
        bindings["Mem"] = Mem
        bindings["MemEntry"] = MemEntry
        bindings["MemMatch"] = MemMatch
    if has_plan_mode_tools:
        bindings["enter_plan_mode"] = _enter_plan_mode
        bindings["exit_plan_mode"] = _exit_plan_mode
    if not _headless:
        bindings["ask"] = _ask

    # Fail fast if an exported tool would overwrite a reserved global/builtin.
    reserved_names = set(dir(builtins))
    reserved_names.update(bindings.keys())
    reserved_names.add("__agent_id__")
    collisions = sorted(name for name in tools_mod.__all__ if name in reserved_names)
    if collisions:
        names = ", ".join(collisions)
        raise RuntimeError(f"Tool name collision with reserved REPL names: {names}")

    # Publish synthetic module and implicitly import all tools into globals.
    sys.modules["tools"] = tools_mod
    _ns.update(bindings)
    for name in tools_mod.__all__:
        _ns[name] = exported_tools[name]

    # Auto-await bare imported tools and tools.<name>(...) calls.
    _async_tool_names.clear()
    _async_tool_names.update(tools_mod.__all__)
    _async_tool_names.add("search_skills")
    if not _headless:
        _async_tool_names.add("ask")
    if has_plan_mode_tools:
        _async_tool_names.add("enter_plan_mode")
        _async_tool_names.add("exit_plan_mode")
    _async_method_names = set(_BASE_ASYNC_METHOD_NAMES)
    _async_method_names.update(tools_mod.__all__)

    _tools_initialized = True


# Flag that lets exec/eval accept top-level `await` (CPython 3.10+).
_ASYNC_FLAG = ast.PyCF_ALLOW_TOP_LEVEL_AWAIT

# Names of async tool functions — auto-awaited if the LLM forgets `await`.
_async_tool_names = set()


# Async method names on wrapper objects (Task, ShellHandle, Skill, etc.)
_BASE_ASYNC_METHOD_NAMES = {
    # Task
    "claim", "start", "done", "cancel", "delete", "block", "wait_on", "update",
    # ShellHandle
    "result", "write", "output", "kill",
    # SkillSummary
    "load",
    # Skill
    "read_file",
}
_async_method_names = set(_BASE_ASYNC_METHOD_NAMES)


class _AutoAwait(ast.NodeTransformer):
    """Inject `await` around calls to known async tool functions and methods.

    Skips calls already inside an `await` or passed directly to
    asyncio.gather / create_task / ensure_future / wait.
    """

    def __init__(self, names):
        self._names = names
        self._inside_await = False
        self._suppress = False

    def visit_Await(self, node):
        old = self._inside_await
        self._inside_await = True
        node = self.generic_visit(node)
        self._inside_await = old
        return node

    @staticmethod
    def _is_async_passthrough(node):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            if isinstance(node.func.value, ast.Name) and node.func.value.id == "asyncio":
                return node.func.attr in ("gather", "create_task", "ensure_future", "wait")
        return False

    def _should_auto_await(self, node):
        """Check if a call node should be auto-awaited."""
        if isinstance(node.func, ast.Name):
            return node.func.id in self._names
        if isinstance(node.func, ast.Attribute):
            return node.func.attr in _async_method_names
        return False

    def visit_Call(self, node):
        if self._is_async_passthrough(node):
            old = self._suppress
            self._suppress = True
            node = self.generic_visit(node)
            self._suppress = old
            return node
        node = self.generic_visit(node)
        if (not self._inside_await
                and not self._suppress
                and self._should_auto_await(node)):
            return ast.Await(value=node)
        return node


def _displayhook(value):
    """Custom displayhook matching interactive Python behavior.

    Skips None and _Awaitable sentinels, sets builtins._, prints repr.
    """
    if value is None or isinstance(value, _Awaitable):
        return
    builtins._ = value
    print(repr(value))


async def _handle_exec(exec_id, code):
    """Execute code using real REPL semantics (ast.Interactive + "single" mode)."""
    global _done_requested
    _done_requested = False
    stdout_buf = io.StringIO()
    old_stdout, old_stderr = sys.stdout, sys.stderr
    old_displayhook = sys.displayhook
    sys.stdout = sys.stderr = stdout_buf
    sys.displayhook = _displayhook
    error = None

    # Mute fd 2 during parse/compile to suppress C-level SyntaxWarnings
    _saved_fd = _mute_stderr()
    try:
        tree = ast.parse(code)
    except SyntaxError:
        error = traceback.format_exc()
        tree = None

    if tree and _async_tool_names:
        tree = _AutoAwait(_async_tool_names).visit(tree)
        ast.fix_missing_locations(tree)

    if tree:
        for node in tree.body:
            try:
                mod = ast.Interactive(body=[node])
                ast.fix_missing_locations(mod)
                co = compile(mod, "<repl>", "single", flags=_ASYNC_FLAG)
                _unmute_stderr(_saved_fd)
                _saved_fd = None
                if co.co_flags & inspect.CO_COROUTINE:
                    await types.FunctionType(co, _ns)()
                else:
                    exec(co, _ns)
                if _done_requested:
                    break
                _saved_fd = _mute_stderr()
            except Exception:
                error = traceback.format_exc()
                break
    if _saved_fd is not None:
        _unmute_stderr(_saved_fd)

    sys.stdout, sys.stderr = old_stdout, old_stderr
    sys.displayhook = old_displayhook

    output = _truncate_output(stdout_buf.getvalue())
    _send({
        "type": "exec_result",
        "id": exec_id,
        "output": output,
        "response": "",
        "error": error,
    })


def _handle_snapshot(snap_id):
    """Serialize the REPL namespace using dill."""
    skip = {
        "json",
        "asyncio",
        "dill",
        "print",
        "done",
        "list_tools",
        "search_tools",
        "search_skills",
        "search_history",
        "search_mem",
        "reset_repl",
        "ask",
        "tools",
        "ToolInfo",
        "HistoryMatch",
        "MemMatch",
    }
    skip.update(t["name"] for t in _tool_defs)

    data = {}
    for k, v in _ns.items():
        if k.startswith("_") or k in skip:
            continue
        try:
            dill.dumps(v)
            data[k] = v
        except Exception:
            continue
    _send({"type": "snapshot_result", "id": snap_id, "data": dill.dumps(data).hex()})


def _handle_restore(restore_id, data_str):
    """Restore namespace from a dill snapshot."""
    saved = dill.loads(bytes.fromhex(data_str))
    _ns.update(saved)
    _send({"type": "exec_result", "id": restore_id, "output": "", "response": "", "error": None})


def _handle_reset(reset_id):
    """Reset the REPL namespace via the protocol."""
    _reset_repl()
    _send({"type": "reset_result", "id": reset_id})


def _check_complete(code):
    """Check if code is syntactically complete, suppressing warnings."""
    saved = _mute_stderr()
    try:
        ast.parse(code)
        return True
    except SyntaxError:
        return False
    finally:
        _unmute_stderr(saved)
