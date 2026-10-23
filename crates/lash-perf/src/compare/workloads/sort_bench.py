def benchmark(n):
    result = 0
    for trip in range(n):
        data = []
        for i in range(1000):
            data.append(str(1000-i))
        data.sort()
        for i in range(1,1000):
            if data[i-1] > data[i]:
                raise ValueError("sort order")
        result += len(data)
    return result
