def benchmark(n):
    items = []
    for i in range(n):
        items.append({"id": i, "key": (i * 7919) % n})
    items.sort(key=lambda item: item["key"])
    return items[1]["id"] + items[n - 1]["key"]
