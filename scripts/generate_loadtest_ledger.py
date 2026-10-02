#!/usr/bin/env python3
"""Render the witness CHECKs and Python vocabulary from the Rust ledger unions."""
import argparse
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = ROOT / 'scripts/loadtest-ledger.json'
SQL = ROOT / 'runbooks/restate-postgres-workers/witness.sql'


def render_sql(source, contract):
    for table, columns, pairs in [('witness_load_events', ('operation', 'phase'), contract['events']),
                                  ('witness_load_faults', ('kind', 'phase'), contract['faults'])]:
        start = source.index(f'CREATE TABLE {table} (')
        end = source.index('\n);', start)
        body = re.sub(r',\n    CONSTRAINT load_\w+_pair CHECK \(.*?\n    \)$', '', source[start:end], flags=re.S)
        predicate = ' OR\n        '.join(f"({columns[0]} = '{left}' AND {columns[1]} = '{right}')" for left, right in pairs)
        body += f',\n    CONSTRAINT load_{columns[0]}_pair CHECK (\n        {predicate}\n    )'
        source = source[:start] + body + source[end:]
    return source


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--generator', type=Path, required=True)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    contract = json.loads(subprocess.check_output([str(args.generator.resolve())], text=True, cwd=ROOT))
    outputs = {CONTRACT: json.dumps(contract, indent=2) + '\n', SQL: render_sql(SQL.read_text(), contract)}
    if args.check:
        stale = [str(path.relative_to(ROOT)) for path, text in outputs.items() if path.read_text() != text]
        if stale:
            raise SystemExit('stale load ledger contracts: ' + ', '.join(stale))
    else:
        for path, text in outputs.items():
            path.write_text(text)


if __name__ == '__main__':
    main()
