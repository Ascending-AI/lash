def benchmark(n):
    result = 0.0
    for j in range(n):
        value = 0.0
        a = 0.1
        for i in range(1000):
            value += a * a
            a += 1.1
        result += value
    return result
