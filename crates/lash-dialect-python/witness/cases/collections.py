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
