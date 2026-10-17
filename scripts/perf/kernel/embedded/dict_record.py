counts = {}
for n in range(2000):
    key = "key_" + str(n % 64)
    counts[key] = counts.get(key, 0) + n
result = counts
