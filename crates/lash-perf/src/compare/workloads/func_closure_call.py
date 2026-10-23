def benchmark(n):
    result = 0
    def f(a):
        nonlocal result
        result += 1
    for j in range(n):
        f(j)
        f(j)
        f(j)
        f(j)
    return result
