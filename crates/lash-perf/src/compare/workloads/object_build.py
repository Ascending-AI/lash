def benchmark(n):
    items = []
    for i in range(n):
        items.append({"id": i, "key": (i * 7919) % n})
    return len(items) + items[n - 1]["key"]
