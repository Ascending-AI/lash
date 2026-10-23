def f(a):
    return 1

def benchmark(n):
    result = 0
    for j in range(n):
        result += f(j)
        result += f(j)
        result += f(j)
        result += f(j)
    return result
