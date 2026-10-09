def classify(n):
    if n < 0:
        return "negative"
    elif n == 0:
        return "zero"
    elif n < 10:
        return "small"
    else:
        return "large"
print([classify(n) for n in [-5, 0, 3, 42]])
for n in range(5):
    if n == 1:
        continue
    if n == 3:
        break
    print("for", n)
else:
    print("no break")
for n in range(2):
    print("loop", n)
else:
    print("ran to the end")
n = 0
while n < 10:
    n += 3
    if n % 2 == 0:
        continue
    print("while", n)
else:
    print("while ended at", n)
while True:
    n -= 1
    if n < 8:
        break
else:
    print("never")
print(n)
total = 0
for i in range(1, 4):
    for j in range(i):
        if j == 2:
            break
        total += i * j
    else:
        total += 100
print(total)
i = 10
for i in range(3):
    pass
print(i, [i for i in range(2)], i)
def find(xs, wanted):
    for index, x in enumerate(xs):
        if x == wanted:
            return index
    return -1
print(find(["a", "b"], "b"), find([], 1), 1 < 2 < 3, 1 < 2 > 5, 3 > 2 >= 2 != 1)
count = 0
def tick():
    global count
    count += 1
    return count
print(1 < tick() < 3, tick() > 5 > tick(), count)
x = 5
x -= 2
x *= 4
x //= 5
x **= 3
x %= 5
x /= 2
print(x, 3 if x else 4, None is None, [] is [], [] == [], (1, 2) == (1, 2), "a" != "b", x is not None)
for word in "ab":
    for pair in [(1, 2), (3, 4)]:
        first, second = pair
        print(word, first + second)
a, (b, c), d = 1, (2, 3), [4]
a, b = b, a
print(a, b, c, d, list(range(3)), list(range(5, 0, -2)), len(range(10)), list(range(0)), list(range(3, 1)))
assert a == 2, "a is two"
try:
    assert b == 2, "b is not two"
except AssertionError as error:
    print("assert", error)
