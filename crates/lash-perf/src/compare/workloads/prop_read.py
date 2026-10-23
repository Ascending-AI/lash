def benchmark(n):
    obj = {"a":1,"b":2,"c":3,"d":4}
    result = 0
    for j in range(n):
        result += obj["a"]
        result += obj["b"]
        result += obj["c"]
        result += obj["d"]
    return result
