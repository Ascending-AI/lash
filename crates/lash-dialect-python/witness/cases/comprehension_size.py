d = {"a": 1}
def add(name):
    d["z" + name] = 1
    return name
try:
    print([add(name) for name in d])
except RuntimeError as error:
    print("RuntimeError", error)
print(len(d))
s = {1}
def grow(x):
    s.add(x + 10)
    return x
try:
    print({grow(x) for x in s})
except RuntimeError as error:
    print("RuntimeError", error)
print(sorted(s))
