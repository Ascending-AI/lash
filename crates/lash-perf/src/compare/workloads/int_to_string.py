def benchmark(n):
    result = ""
    for j in range(n):
        result = str(j % 10)
        result = str(j % 100)
        result = str(j)
    return len(result)
