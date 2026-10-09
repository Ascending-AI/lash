xs = [3, 1, 2]
ys = xs.copy()
ys.clear()
print(xs, ys, xs.index(1), xs.count(3), xs.pop(1), xs)
d = {"a": 1, "b": 2}
e = d.copy()
e.clear()
print(d, e, d.pop("a"), d.setdefault("b", 9), d.setdefault("c"), d, list(d.items()))
s = {1, 2}
t = s.copy()
t.remove(1)
t.clear()
print(sorted(s), len(t), sorted(s.union({3})), sorted(s.intersection({2, 3})), sorted(s.difference({2})))
try:
    s.remove(99)
except KeyError as error:
    print("KeyError", error)
word = "banana"
print(word.count("an"), word.index("n"), word.find("x"), word.replace("a", "o"), word.upper().lower(), word.startswith("ba"), word.endswith("x"))
try:
    word.index("x")
except ValueError as error:
    print(error)
print("  a b  ".strip() + "|" + "  a".lstrip() + "|" + "a  ".rstrip() + "|", "a-b-c".split("-"), "+".join(["1", "2"]), "x y  z".split())
try:
    xs.pop(10)
except IndexError as error:
    print(error)
try:
    [].pop()
except IndexError as error:
    print(error)
