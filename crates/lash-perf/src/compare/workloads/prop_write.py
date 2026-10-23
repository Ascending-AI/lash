def benchmark(n):
    obj = {"a":1,"b":2,"c":3,"d":4}
    for j in range(n):
        obj["a"] = j
        obj["b"] = j
        obj["c"] = j
        obj["d"] = j
    return obj["a"]
