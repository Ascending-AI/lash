//! A deterministic SEP-3415 server for laws and examples: a stdio MCP
//! server whose events live in a directory (its "world") that outlives
//! every server process and every node.
//!
//! The world holds `events.jsonl`, one occurrence a line, in order; the
//! position after the `n`th is the cursor `"n"`. `control.json` steers the
//! server, read at every request:
//!
//! - `null_cursor`: `"now"` (SEP-3415's start from now, the default) or
//!   `"origin"`, which replays from the first retained event;
//! - `retained_from`: the first retained position; a cursor before it
//!   resumes there with `truncated: true`;
//! - `rewind_once`: the next poll or stream that resumes from a cursor
//!   resumes this many positions earlier, redelivering what it delivered;
//! - `refuse`: `{code, message}` every events request answers;
//! - `terminate`: `{code, message}` an open stream ends with;
//! - `next_poll_ms` and `heartbeat_ms`: the server's pacing.
//!
//! `requests.jsonl` logs every request and notification the server read,
//! once it is handled: a logged poll or stream has fixed where it starts.

#![allow(
    clippy::disallowed_methods,
    reason = "FIG-2971: the fixture is a test host; its world is the directory the test passes in"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::config::McpEventsConfig;
use crate::config::{McpServerConfig, McpStdioTransport};

/// The one event type the fixture offers.
pub const FIXTURE_EVENT: &str = "ticket.created";

const SERVER: &str = r#"
import json, os, sys, threading, time

world = os.environ['EVENTS_WORLD']
lock = threading.Lock()
streams = {}

def path(name):
    return os.path.join(world, name)

def send(message):
    with lock:
        sys.stdout.write(json.dumps(message, separators=(',', ':')) + '\n')
        sys.stdout.flush()

def read_json(name, default):
    try:
        with open(path(name), 'r', encoding='utf-8') as f:
            return json.load(f)
    except (FileNotFoundError, ValueError):
        return default

def control():
    return read_json('control.json', {})

def take_rewind():
    with lock:
        c = control()
        rewind = c.pop('rewind_once', 0)
        if rewind:
            with open(path('control.json'), 'w', encoding='utf-8') as f:
                json.dump(c, f)
        return rewind

def events():
    try:
        with open(path('events.jsonl'), 'rb') as f:
            # The newline publishes an occurrence. A concurrent append may
            # still be writing the tail, including a UTF-8 code point.
            return [json.loads(line) for line in f if line.endswith(b'\n') and line.strip()]
    except FileNotFoundError:
        return []

def start_at(cursor):
    c = control()
    held = events()
    if cursor is None:
        start = 0 if c.get('null_cursor') == 'origin' else len(held)
    else:
        start = max(0, int(cursor) - take_rewind())
    retained = c.get('retained_from', 0)
    if start < retained:
        return retained, True
    return start, False

def error(request_id, refusal):
    send({'jsonrpc': '2.0', 'id': request_id, 'error': {
        'code': refusal['code'], 'message': refusal['message']}})

def stream(request_id, position):
    meta = {'io.modelcontextprotocol/subscriptionId': request_id}
    last_beat = 0.0
    while streams.get(request_id):
        c = control()
        if 'terminate' in c:
            send({'jsonrpc': '2.0', 'method': 'notifications/events/terminated', 'params': {
                'error': c['terminate'], '_meta': meta}})
            send({'jsonrpc': '2.0', 'id': request_id, 'result': {}})
            return
        held = events()
        while position < len(held) and streams.get(request_id):
            event = dict(held[position])
            position += 1
            event['cursor'] = str(position)
            event['_meta'] = meta
            send({'jsonrpc': '2.0', 'method': 'notifications/events/event', 'params': event})
        now = time.monotonic()
        if now - last_beat >= c.get('heartbeat_ms', 200) / 1000.0:
            last_beat = now
            send({'jsonrpc': '2.0', 'method': 'notifications/events/heartbeat', 'params': {
                'cursor': str(position), '_meta': meta}})
        time.sleep(0.02)

def log(line):
    with open(path('requests.jsonl'), 'a', encoding='utf-8') as f:
        f.write(line)

for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    request_id = message.get('id')
    params = message.get('params') or {}
    if method == 'server/discover':
        send({'jsonrpc': '2.0', 'id': request_id, 'result': {
            'supportedVersions': ['2026-07-28'], 'resultType': 'complete', 'ttlMs': 0,
            'cacheScope': 'private',
            'capabilities': {'tools': {}, 'extensions': {'io.modelcontextprotocol/events': {}}},
            'serverInfo': {'name': 'events-fixture', 'version': '1.0.0'}}})
    elif method == 'tools/list':
        send({'jsonrpc': '2.0', 'id': request_id, 'result': {'tools': []}})
    elif method == 'events/list':
        send({'jsonrpc': '2.0', 'id': request_id, 'result': {'events': [{
            'name': 'ticket.created', 'description': 'A ticket was created.',
            'delivery': control().get('delivery', ['poll', 'push']),
            'inputSchema': {'type': 'object'},
            'payloadSchema': {'type': 'object'}}]}})
    elif method in ('events/poll', 'events/stream'):
        c = control()
        if 'refuse' in c:
            error(request_id, c['refuse'])
        elif params.get('name') != 'ticket.created':
            error(request_id, {'code': -32023, 'message': 'no such event'})
        elif method == 'events/poll':
            position, truncated = start_at(params.get('cursor'))
            held = events()
            batch = held[position:position + int(params.get('maxEvents') or 50)]
            after = position + len(batch)
            send({'jsonrpc': '2.0', 'id': request_id, 'result': {
                'events': batch, 'cursor': str(after), 'truncated': truncated,
                'hasMore': after < len(held), 'nextPollMs': c.get('next_poll_ms', 100)}})
        else:
            streams[request_id] = True
            position, truncated = start_at(params.get('cursor'))
            send({'jsonrpc': '2.0', 'method': 'notifications/events/active', 'params': {
                'cursor': str(position), 'truncated': truncated,
                '_meta': {'io.modelcontextprotocol/subscriptionId': request_id}}})
            threading.Thread(target=stream, args=(request_id, position), daemon=True).start()
    elif method == 'notifications/cancelled':
        streams.pop(params.get('requestId'), None)
    elif request_id is not None and method is not None:
        send({'jsonrpc': '2.0', 'id': request_id, 'error': {
            'code': -32601, 'message': 'method not found'}})
    log(line)
"#;

/// A fixture server's world: its events, its steering and its log.
#[derive(Clone, Debug)]
pub struct McpEventsFixture {
    world: PathBuf,
}

impl McpEventsFixture {
    /// A world in `dir`, which must exist.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { world: dir.into() }
    }

    /// The world's directory.
    pub fn world(&self) -> &Path {
        &self.world
    }

    /// A stdio server over this world, its events offered under `events`.
    pub fn server(&self, events: McpEventsConfig) -> McpServerConfig {
        let mut transport = McpStdioTransport::new(
            "python3",
            vec!["-u".to_owned(), "-c".to_owned(), SERVER.to_owned()],
        );
        transport.env =
            BTreeMap::from([("EVENTS_WORLD".to_owned(), self.world.display().to_string())]);
        McpServerConfig::stdio(transport).with_events(events)
    }

    /// Append an occurrence of the fixture's event, `eventId` `id`.
    ///
    /// # Panics
    ///
    /// The world cannot be written.
    #[expect(clippy::expect_used, reason = "a fixture's world is the test's own")]
    pub fn emit(&self, id: &str, data: Value) {
        use std::io::Write as _;
        let line = json!({
            "eventId": id,
            "name": FIXTURE_EVENT,
            "timestamp": "2026-10-14T00:00:00Z",
            "data": data,
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.world.join("events.jsonl"))
            .expect("the world's events open");
        // Format before publishing: Value's Display writes JSON in fragments.
        file.write_all(format!("{line}\n").as_bytes())
            .expect("the event is written");
    }

    /// Replace the server's steering with `control`.
    ///
    /// # Panics
    ///
    /// The world cannot be written.
    #[expect(clippy::expect_used, reason = "a fixture's world is the test's own")]
    pub fn control(&self, control: Value) {
        let staged = self.world.join("control.json.next");
        std::fs::write(&staged, control.to_string()).expect("the control is written");
        std::fs::rename(staged, self.world.join("control.json")).expect("the control is swapped");
    }

    /// Every request and notification the server read, in order.
    pub fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.world.join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The requests and notifications of `method` the server read.
    pub fn requests_of(&self, method: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request.get("method").and_then(Value::as_str) == Some(method))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    /// EVENT-PUBLICATION: a reader sees only newline-terminated occurrences,
    /// even when a producer is stopped partway through writing one.
    #[tokio::test]
    async fn an_incomplete_event_is_not_published_until_its_line_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = McpEventsFixture::new(dir.path());
        fixture.emit("first", json!({}));
        let mut events = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("events.jsonl"))
            .unwrap();
        let event = json!({
            "eventId": "second", "name": FIXTURE_EVENT,
            "timestamp": "2026-10-14T00:00:00Z", "data": {},
        })
        .to_string();
        events.write_all(&event.as_bytes()[..1]).unwrap();
        let mut server = tokio::process::Command::new("python3")
            .args(["-u", "-c", SERVER])
            .env("EVENTS_WORLD", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = server.stdin.take().unwrap();
        let mut output = BufReader::new(server.stdout.take().unwrap()).lines();
        for (id, expected) in [
            (1, json!([])),
            (2, json!([])),
            (3, json!([serde_json::from_str::<Value>(&event).unwrap()])),
        ] {
            if id == 2 {
                // Complete JSON alone is not the publication boundary.
                events.write_all(&event.as_bytes()[1..]).unwrap();
            } else if id == 3 {
                events.write_all(b"\n").unwrap();
            }
            let request = json!({
                "jsonrpc": "2.0", "id": id, "method": "events/poll",
                "params": { "name": FIXTURE_EVENT, "cursor": "1" },
            });
            input
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let line = output
                .next_line()
                .await
                .unwrap()
                .expect("a partial event must not crash its reader");
            let response: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["result"]["events"], expected);
            assert_eq!(
                response["result"]["cursor"],
                if id == 3 { "2" } else { "1" }
            );
        }
        drop(input);
        assert!(server.wait().await.unwrap().success());
    }
}
