def benchmark(n):
    result = 0
    for j in range(n):
        value = 0
        for i in range(1000):
            value += i * i
        result += value
    return result
