xs = [5, 3, 8, 1]
print(xs[0], xs[-1], xs[1:3], xs[:2], xs[2:], xs[::-1], xs[::2], xs[-3:-1], xs[10:], xs[1:100], xs[3:0:-1])
text = "hello world"
print(text[0], text[-1], text[1:4], text[::-1], text[::3], len(text), "lo" in text, "z" not in text)
pair = (1, "two", 3.0)
print(pair[1], pair[-1], pair[:2], pair + (4,), len(pair), pair * 2, 3.0 in pair)
xs.append(9)
xs.extend([2, 2])
xs.insert(1, 7)
xs.insert(-100, 0)
xs.insert(100, 6)
print(xs, xs.pop(), xs.pop(0), xs.index(8), xs.count(2), 2 in xs)
xs.remove(2)
xs.sort()
ys = xs
ys.reverse()
print(xs, ys is xs, sorted(xs), xs.copy() == xs, sum(xs), min(xs), max(xs))
xs += [1]
ys = ys + [0]
print(xs, ys, [0] * 3, [1, 2] * 2, "ab" * 3, 2 * "x", [[0] * 2] * 2)
try:
    xs[99]
except IndexError as error:
    print(error)
try:
    xs.remove(1000)
except ValueError as error:
    print(error)
try:
    (1, 2)[0] = 5
except TypeError as error:
    print("tuples are immutable")
d = {"b": 2, "a": 1}
d["c"] = 3
d["b"] = 20
print(d, list(d), list(d.keys()), list(d.values()), list(d.items()), len(d), "a" in d, 1 in d)
print(d.get("z"), d.get("z", 0), d.setdefault("z", []), d.pop("a"), d)
d.update({"b": 0, "y": 1})
del d["z"]
e = dict(d)
e["new"] = True
print(d, e, d == {"y": 1, "c": 3, "b": 0}, {1: "int"}[1.0], {} == dict(), dict([("k", "v")]))
seen = []
def mark(label, value):
    seen.append(label)
    return value
duplicates = {mark('first key', 1): mark('first value', 'one'), mark('second key', 1.0): mark('second value', 'float')}
print(duplicates, seen)
try:
    invalid = {mark('valid key', 'a'): mark('valid value', 1), mark('invalid key', []): mark('invalid value', 2)}
except TypeError:
    print('unhashable key', seen)
s = {3, 1, 2, 3}
s.add(4)
s.discard(1)
s.discard(100)
print(sorted(s), len(s), 2 in s, sorted(s | {9}), sorted(s & {2, 3, 7}), sorted(s - {2}), sorted(s ^ {2, 5}), sorted(set("aab")), set() == set([]), {1, 2} == {2, 1}, sorted(s.union([7])), sorted(s.intersection([2, 3])), sorted(s.difference([2])))
print([x * 2 for x in range(5) if x % 2], {x: x * x for x in range(3)}, sorted({x % 2 for x in range(5)}), [(x, y) for x in range(2) for y in "ab"])
print(list(zip([1, 2, 3], "ab")), list(enumerate(["a", "b"], 1)), list(reversed([1, 2, 3])), tuple([1, 2]), list((1, 2)), list("ab"), sum(x for x in range(4)))
matrix = [[1, 2], [3, 4]]
matrix[0][1] = 20
matrix[1][0] += 5
print(matrix, [row[0] for row in matrix], [n for row in matrix for n in row], matrix[1][-1])
print("a,b,,c".split(","), "  two  words ".split(), " pad ".strip() + "|", "Hello".upper(), "Hello".lower(), "hello".replace("l", "L"), "hello".find("l"), "hello".find("z"), "hello".startswith("he"), "hello".endswith("lo"), "hello".count("l"), ", ".join(str(n) for n in range(3)))
print(str(1.0), str(None), str(True), repr("it's"), repr('say "hi"'), repr("tab\there"), int("7") + 1, float(3), str([1.5, "x"]), isinstance(1, int), isinstance(True, int), isinstance(1.0, (int, str)), isinstance("s", str), type(1).__name__, type([]).__name__, type(None).__name__)
print(ord("a"), chr(98), abs(-3.5), abs(4), len(""), len([[], []]), bool("False"), 10 in range(11), [1, 2] < [1, 3], (1, 2) < (1, 2, 0), "abc" < "abd", min("b", "a"), max([1, 5, 3]))

# A native loop cursor reads the live list after a park and at exhaustion.
walked = [1, 2]
seen = []
for item in walked:
    seen.append(await host.echo(item))
    if item == 1:
        walked.append(3)
    elif item == 2:
        walked[2] = 4
else:
    print('exhausted', seen)
for item in walked:
    print(item)
    walked.clear()
else:
    print('cleared')
for pair in ((1, 2), (3, 4)):
    print(await host.echo(pair[0]), pair[1])

# Proven collection membership keeps numeric key equality and deep equality.
keys = {1: 'yes', (1, 2): 'pair'}
print(1.0 in keys, (1, 2) not in keys, 9 not in keys)
members = {1, 2}
print(1.0 in members, 3 not in members)
boxes = [[1], [2]]
print([1] in boxes, [3] not in boxes)

# A sort callback keeps its default binding and captured writes.
ranks = []
def rank(item, offset=10):
    ranks.append(item)
    return offset - item
ordered = [3, 1, 2]
ordered.sort(key=rank)
print(ordered, ranks, rank(item=4), ranks)

# Escaping fixed signatures retain positional-only, defaults and keyword errors.
def make_fixed():
    def fixed(a, /, b=7):
        return (a, b)
    return fixed
fixed = make_fixed()
print(fixed(2), fixed(3, b=8))
for args, kwargs in [([], {}), ([1, 2, 3], {}), ([1, 2], {"b": 3}), ([1], {"a": 2}), ([1], {"other": 2})]:
    try:
        fixed(*args, **kwargs)
    except TypeError:
        print('fixed argument error')

# Dynamic integer indices keep negative and out-of-range Python behavior.
for index in (0, 1, -1, -3, 3):
    try:
        print([10, 20][index])
    except IndexError:
        print('dynamic index error')

# Proven range indices use the same Python negative-index and bounds rules.
def read_by_range():
    values = [10, 20]
    for index in range(-3, 3):
        try:
            print(values[index])
        except IndexError:
            print('range index error')
read_by_range()
print(sorted([], key=1), sorted([3, 11, 2], key=str))
try:
    sorted([1], key=1)
except TypeError:
    print('invalid nonempty sort key')
summed = [[1], [2]]
print(sum(summed, []), summed, sum((1, 2, 3), 4))

# A packed expression body can return a closure over its bound argument.
factory = lambda value: (lambda: value)
reader = factory(await host.echo(17))
await host.echo('after binding')
print(reader())

# Reused callback inputs cannot alias captured arguments or variadic tuples.
def capture_key(item, *rest, **keywords):
    def captured():
        return item
    ranks.append((captured, rest, keywords))
    return item
ranks = []
print(sorted([3, 1, 2], key=capture_key))
await host.echo('captured sort arguments')
print([(captured(), rest, keywords) for captured, rest, keywords in ranks])
print(sum([1, 2.5, 3]), sum([1, 2], 0.5))
try:
    sum([1, 'bad'])
except TypeError:
    print('invalid sum item')

# Membership must retain dynamic behavior when an annotation is inaccurate.
def membership_hint(items: dict):
    return 2 in items
print(membership_hint([1, 2]))

# In-place sorting preserves the receiver alias and stable equal-key order.
stable_rows = [{'rank': 2, 'id': 'a'}, {'rank': 1, 'id': 'b'}, {'rank': 2, 'id': 'c'}]
stable_alias = stable_rows
stable_rows.sort(key=lambda row: row['rank'])
print([row['id'] for row in stable_rows], stable_alias is stable_rows)
stable_rows.sort(key=lambda row: row['rank'], reverse=True)
print([row['id'] for row in stable_alias])
