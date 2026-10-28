import json

value = json.loads('{"z": ["\\u00e9", "\\ud83d\\ude00", -0.0], "a": 123456789012345678901234567890}')
print(list(value), value['z'], value['a'])
print(json.dumps(value, separators=(',', ':')))
print(json.dumps(value, ensure_ascii=False, separators=(',', ':')))
print(json.dumps({'z': 1, 'a': 2}, sort_keys=True))
source = '[' * 40 + '1' + ']' * 40
value = json.loads(source)
for i in range(40):
    value = value[0]
print(value)
print(await host.echo(json.dumps({'b': [True, None, 1.0, -0.0], 'a': 'é'})))

print(json.dumps({'text': '\x7f\n"\\', 'numbers': [1.0, 1e-5, 1e16, -0.0]}, sort_keys=True))
print(json.dumps({'é': 1.0}, separators=(' / ', ' = ')))
print(json.dumps({1: 'one', None: 'null', False: 'false'}))
print(json.dumps([float('inf'), float('-inf'), float('nan')]))
try:
    print(json.dumps(float('inf'), allow_nan=False))
except ValueError:
    print('nonfinite')
print(json.dumps([1, 2], indent=2))
def convert(value):
    return 'converted'
print(json.dumps({1, 2}, default=convert))
cycle = []
cycle.append(cycle)
try:
    print(json.dumps(cycle))
except ValueError:
    print('circular')

# Token scanning preserves escapes and both sides of the inline integer range.
print(json.loads(json.dumps(['a"b', 'a\\b', 'line\nbreak', 'é'], ensure_ascii=False)))
print(json.loads('[0,-0,9223372036854775807,-9223372036854775808,9223372036854775808,-9223372036854775809]'))
print(json.loads('[1.0,1e0,-0.0,1E-9]'))
for invalid in ['01', '-01', '1.', '1e', '1e+', '+1', '.5']:
    try:
        json.loads(invalid)
    except ValueError:
        print('invalid number', invalid)
