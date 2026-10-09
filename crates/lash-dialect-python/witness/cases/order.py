log = []
def note(tag, value):
    log.append(tag)
    return value
x = 1
def bump():
    global x
    x += 10
    return x
print(x + bump(), bump() + x, x, [x, bump(), x], (x, bump()), {note("k", "key"): note("v", x)})
print(note("a", 1) + note("b", 2) * note("c", 3), log)
log.clear()
items = [0, 0, 0]
items[note("index", 1)] = note("value", 5)
print(items, log)
log.clear()
def pick(a, b, c):
    return (a, b, c)
print(pick(note("1", x), bump(), note("3", x)), log)
log.clear()
print(note("left", 0) and note("skipped", 1), note("left2", 0) or note("right", 2), note("t", 1) if note("cond", True) else note("e", 2), log)
log.clear()
d = {}
d[note("key", "k")] = [note("first", 1), note("second", 2)]
d["k"][note("i", 0)] += note("add", 10)
print(d, log)
log.clear()
f = note
f = [note, bump][note("choose", 0)]
print(f("called", "result"), log, f"{note('f1', 1)}{bump()}{note('f2', x)}", note("m", "a").upper() + note("n", "b"))
value = [1, 2, 3]
alias = value
value = value + [note("rebind", 4)]
alias += [5]
print(value, alias, log[-1])
