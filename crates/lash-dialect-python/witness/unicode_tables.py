#!/usr/bin/env python3
"""Write Python 3.12's Unicode 15.0 text predicates as kernel helpers."""
import pathlib
import unicodedata

if unicodedata.unidata_version != '15.0.0':
    raise SystemExit('use CPython with Unicode 15.0.0')


def ranges(predicate):
    found = []
    start = None
    for point in range(0x110001):
        matches = point < 0x110000 and predicate(chr(point))
        if matches and start is None:
            start = point
        if not matches and start is not None:
            found.append((start, point - 1))
            start = None
    return found


header = '''# Python 3.12, Unicode 15.0.0. Re-record with witness/unicode_tables.py.
# These are Python predicates, including its four extra ASCII whitespace
# characters and its treatment of separators, unassigned and private use.

use lt
use le
use sub
use add
use div_floor
use list.len

'''
parts = [header]
for name, predicate in [('whitespace', str.isspace), ('printable', lambda c: not c.isprintable())]:
    rows = ranges(predicate)
    parts.append(f'''function py.text.{name}(point: Int) -> Bool
kernel 1
charge {32 + len(rows) * 8}
body {{
  let ranges = [
''')
    if name == 'printable':
        parts[-1] = parts[-1].replace('  let ranges = [', '''  # The common ASCII case needs no range table.
  if lt(point, 32) { return false }
  if le(point, 126) { return true }
  let ranges = [''')
    parts.extend(f'    ({lo}, {hi}),\n' for lo, hi in rows)
    parts.append('''  ]
  let low = 0
  let high = list.len(ranges)
  while lt(low, high) {
    let middle = div_floor(add(low, high), 2)
    let interval = ranges[middle]
    if lt(point, interval[0]) {
      set high = middle
    } else {
      if lt(interval[1], point) {
        set low = add(middle, 1)
      } else {
''')
    parts.append('        return ' + ('true' if name == 'whitespace' else 'false') + '\n')
    parts.append('''      }
    }
  }
''')
    parts.append('  return ' + ('false' if name == 'whitespace' else 'true') + '\n}\n\n')
zeros = [point for point in range(0x110000) if unicodedata.decimal(chr(point), None) == 0]
parts.append(f'''# Each Unicode decimal alphabet is ten consecutive code points.
function py.text.decimal(point: Int) -> Int
kernel 1
charge {32 + len(zeros) * 8}
body {{
  for zero in [
''')
parts.extend(f'    {zero},\n' for zero in zeros)
parts.append('''  ] {
    if le(zero, point) {
      if lt(point, add(zero, 10)) { return sub(point, zero) }
    }
  }
  return -1
}
''')
(pathlib.Path(__file__).parent.parent / 'src/helpers/unicode.kernel').write_text(''.join(parts))
