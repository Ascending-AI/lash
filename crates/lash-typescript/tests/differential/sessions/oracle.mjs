// The Node session oracle as a service (FIG-3608).
//
// The generated differential sessions are drawn by a seeded generator in
// `lash-protocol-rlm`, which also runs the lash side; this process answers
// for Node. It reads one session per stdin line,
//
//   {"probe": ["name", ...], "cells": [{"source": "...", "reject": null}, ...]}
//
// runs it in a fresh realm under the one cell-to-Script mapping
// (`realm.mjs`), and writes one line per session: the JSON array of Node's
// observation of each cell, in the shape `expectations.json` records.
//
// It refuses any Node other than the stamped version, as the generators do.

import { createInterface } from 'node:readline';

import { observeSession, requirePinnedNode } from './realm.mjs';

requirePinnedNode();

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of lines) {
  if (line.length === 0) continue;
  const session = JSON.parse(line);
  process.stdout.write(`${JSON.stringify(observeSession(session.probe, session.cells))}\n`);
}
