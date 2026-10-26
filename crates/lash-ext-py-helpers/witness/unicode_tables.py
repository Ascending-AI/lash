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
native
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

# Finite pinned scalar tables are packed text, not thousands of AST tuples.
# Predicate rows contain two six-digit hexadecimal endpoints. Mapping rows
# contain a six-digit key and three output scalars, padded with NUL.
import json

def quoted(text):
    return json.dumps(text, ensure_ascii=False).replace('\\u0000', '\\u{0}')

text_parts = [header.replace('use list.len', 'use text.len\nuse text.slice\nuse text.find\nuse int.parse\nuse mul\nuse text.from_code_points')]
for name, predicate in [('alpha', str.isalpha), ('alnum', str.isalnum), ('digit', str.isdigit), ('numeric', str.isnumeric), ('lower', str.islower), ('upper', str.isupper), ('title', str.istitle), ('identifier_start', str.isidentifier), ('identifier_continue', lambda c: ('a' + c).isidentifier()), ('ignorable', lambda c: ('AΣ' + c + 'B').lower()[1] == 'σ' and not (c.islower() or c.isupper() or c.istitle()))]:
    rows = ranges(predicate)
    table = ''.join(f'{lo:06x}{hi:06x}' for lo, hi in rows)
    text_parts.append(f'''function py.text.{name}(point: Int) -> Bool
kernel 1
charge {64 + len(table) * 32}
body {{
  let table = {quoted(table)}
  let low = 0
  let high = {len(rows)}
  while lt(low, high) {{
    let middle = div_floor(add(low, high), 2)
    let offset = mul(middle, 12)
    let lower = int.parse(text.slice(table, offset, add(offset, 6)), 16)
    let upper = int.parse(text.slice(table, add(offset, 6), add(offset, 12)), 16)
    if lt(point, lower) {{ set high = middle }} else {{
      if lt(upper, point) {{ set low = add(middle, 1) }} else {{ return true }}
    }}
  }}
  return false
}}
''')
for name, transform in [('lowercase', str.lower), ('uppercase', str.upper), ('titlecase', str.title), ('casefold', str.casefold)]:
    rows = [(point, transform(chr(point))) for point in range(0x110000) if transform(chr(point)) != chr(point)]
    assert all(len(text) <= 3 for _, text in rows)
    table = ''.join(f'{point:06x}' + text.ljust(3, chr(0)) for point, text in rows)
    text_parts.append(f'''function py.text.{name}(point: Int) -> Text
kernel 1
charge {64 + len(table.encode()) * 32}
body {{
  let table = {quoted(table)}
  let low = 0
  let high = {len(rows)}
  while lt(low, high) {{
    let middle = div_floor(add(low, high), 2)
    let offset = mul(middle, 9)
    let key = int.parse(text.slice(table, offset, add(offset, 6)), 16)
    if lt(point, key) {{ set high = middle }} else {{
      if lt(key, point) {{ set low = add(middle, 1) }} else {{
        let mapped = text.slice(table, add(offset, 6), add(offset, 9))
        let end = text.find(mapped, "\\u{{0}}", 0)
        if lt(end, 0) {{ return mapped }}
        return text.slice(mapped, 0, end)
      }}
    }}
  }}
  return text.from_code_points([point])
}}
''')
(pathlib.Path(__file__).parent.parent / 'src/helpers/text_data.kernel').write_text(''.join(text_parts))
