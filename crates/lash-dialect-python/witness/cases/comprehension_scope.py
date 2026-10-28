j = 99
try:
    print([j for i in [0] if j for j in [1]])
except UnboundLocalError as error:
    print(type(error).__name__, error)
fs = [lambda: (i, j) for i in [0, 1] for j in range(i + 1)]
print([f() for f in fs], j)
groups = []
for batch in [[], [1, 2], [3]]:
    groups.append([lambda: item for item in batch])
    print([item * 2 for item in batch if item % 2])
print([[read() for read in group] for group in groups])
