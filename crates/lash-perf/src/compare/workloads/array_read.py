def benchmark(n):
    a = []
    for i in range(10):
        a.append(i)
    result = 0
    for j in range(n):
        result += a[0]
        result += a[1]
        result += a[2]
        result += a[3]
        result += a[4]
        result += a[5]
        result += a[6]
        result += a[7]
        result += a[8]
        result += a[9]
    return result
