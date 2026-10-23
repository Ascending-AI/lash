def benchmark(n):
    data = {}
    for j in range(n):
        data = {}
        for i in range(1000):
            data[str(i)] = i
        for i in range(1000):
            if str(i) not in data:
                raise ValueError("bug in map")
    return len(data)
