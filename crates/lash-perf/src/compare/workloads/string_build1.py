def benchmark(n):
    result = ""
    for j in range(n):
        result = ""
        for i in range(1000):
            result += "x"
    return len(result)
