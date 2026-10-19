counts = {}
for n in range(2000):
    key = f"key_{n % 64}"
    counts[key] = counts.get(key, 0) + n
result = counts
