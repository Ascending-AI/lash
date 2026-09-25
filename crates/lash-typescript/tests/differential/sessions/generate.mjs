// The Node session oracle's generator (FIG-3599).
//
// Reads `corpus/<id>.txt` — one file per session, so two lanes' corpus rows
// never share a file (FIG-3727) — and writes `expectations/<id>.json` plus
// `expectations/meta.json`: every session's cells with the reference answer
// of the pinned Node. Each cell runs as a successive classic Script in ONE
// realm under the cell-to-Script mapping `realm.mjs` states once (the README
// next to this file states it in full, and ADR 0062 records it); the
// generated sessions (FIG-3608) are answered by the same realm through
// `oracle.mjs`.
//
// Regeneration is deliberate and byte-identical, as for `../generate.mjs`:
//
//   node crates/lash-typescript/tests/differential/sessions/generate.mjs
//
// The generator refuses any Node other than the stamped version.

import {
  mkdirSync,
  readdirSync,
  readFileSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { NODE_VERSION, observeSession, requirePinnedNode } from './realm.mjs';

requirePinnedNode();

const directory = dirname(fileURLToPath(import.meta.url));

// --- corpus/ -----------------------------------------------------------------

// Every `corpus/<id>.txt` holds exactly one `session <id> ... end` block.
function corpusFiles() {
  const corpusDirectory = join(directory, 'corpus');
  return readdirSync(corpusDirectory)
    .filter((name) => name.endsWith('.txt') && statSync(join(corpusDirectory, name)).isFile())
    .sort()
    .map((name) => {
      const text = readFileSync(join(corpusDirectory, name), 'utf8');
      const sessions = parseCorpus(text, `corpus/${name}`);
      if (sessions.length !== 1 || sessions[0].id !== name.slice(0, -4)) {
        throw new Error(`corpus/${name} must hold exactly the session it is named for`);
      }
      return sessions[0];
    });
}

function parseCorpus(text, file) {
  const sessions = [];
  let session = null;
  let cell = null;
  const lines = text.split('\n');
  if (lines.at(-1) === '') lines.pop();
  const closeCell = () => {
    if (cell) {
      if ((cell.deviation || cell.defect) && !cell.lash) {
        throw new Error(`deviation or defect cell in ${session.id} states no lash answer`);
      }
      delete cell.answered;
      while (cell.lines.length && cell.lines.at(-1) === '') cell.lines.pop();
      cell.source = cell.lines.join('\n') + '\n';
      delete cell.lines;
      session.cells.push(cell);
      cell = null;
    }
  };
  for (const [index, line] of lines.entries()) {
    const where = `${file}:${index + 1}`;
    if (cell && !cell.answered && !/^(cell\b|lash(-resident|-restart)? |end$)/.test(line)) {
      cell.lines.push(line);
      continue;
    }
    if (line === '' || line.startsWith('#')) continue;
    const [keyword, ...rest] = line.split(' ');
    const argument = rest.join(' ');
    if (keyword === 'session') {
      if (session) throw new Error(`${where}: session ${session.id} has no end`);
      session = { id: argument, about: '', host: {}, probe: [], deviations: [], cells: [] };
      continue;
    }
    if (!session) throw new Error(`${where}: \`${keyword}\` outside a session`);
    if (keyword === 'about') {
      session.about = argument;
    } else if (keyword === 'host' && !cell) {
      const [name, ...json] = rest;
      session.host[name] = JSON.parse(json.join(' '));
    } else if (keyword === 'probe') {
      session.probe = rest;
    } else if (keyword === 'deviation' && !cell) {
      session.deviations.push(argument);
    } else if (keyword === 'cell') {
      closeCell();
      if (session.cells.at(-1)?.lash) {
        throw new Error(`${where}: a deviation or defect cell ends its session`);
      }
      const [kind, value] = rest;
      cell = { reject: null, deviation: null, defect: null, lines: [] };
      if (kind === 'reject') cell.reject = value;
      else if (kind === 'deviation') cell.deviation = value;
      else if (kind === 'defect') cell.defect = value;
      else if (kind !== undefined) throw new Error(`${where}: unknown cell kind ${kind}`);
    } else if (keyword === 'lash' || keyword === 'lash-resident' || keyword === 'lash-restart') {
      if (!cell?.deviation && !cell?.defect) {
        throw new Error(`${where}: only a deviation or defect cell states a lash answer`);
      }
      cell.answered = true;
      cell.lash ??= {};
      const answer = JSON.parse(argument);
      if (keyword !== 'lash-restart') cell.lash.resident = answer;
      if (keyword !== 'lash-resident') cell.lash.restart = answer;
    } else if (keyword === 'end') {
      closeCell();
      sessions.push(session);
      session = null;
    } else {
      throw new Error(`${where}: unknown line \`${line}\``);
    }
  }
  if (session) throw new Error(`session ${session.id} has no end`);
  return sessions;
}

// --- generation --------------------------------------------------------------

const sessions = corpusFiles();

// One `expectations/<id>.json` per session plus `meta.json`, so a corpus
// change touches only its own shard; stale shards are removed.
const expectations = join(directory, 'expectations');
mkdirSync(expectations, { recursive: true });
const written = new Set(['meta.json']);
for (const session of sessions) {
  const observations = observeSession(session.probe, session.cells, session.host);
  const cells = session.cells.map((cell, index) => {
    const entry = {
      source: cell.source,
      reject: cell.reject,
      deviation: cell.deviation,
      defect: cell.defect,
      node: observations[index],
    };
    if (cell.lash) entry.lash = cell.lash;
    return entry;
  });
  const output = {
    id: session.id,
    about: session.about,
    ...(Object.keys(session.host).length ? { host: session.host } : {}),
    probe: session.probe,
    deviations: session.deviations,
    cells,
  };
  const name = `${session.id}.json`;
  writeFileSync(join(expectations, name), `${JSON.stringify(output, null, 2)}\n`);
  written.add(name);
}
writeFileSync(
  join(expectations, 'meta.json'),
  `${JSON.stringify({ node: NODE_VERSION }, null, 2)}\n`,
);
for (const name of readdirSync(expectations)) {
  if (name.endsWith('.json') && !written.has(name)) {
    unlinkSync(join(expectations, name));
  }
}
