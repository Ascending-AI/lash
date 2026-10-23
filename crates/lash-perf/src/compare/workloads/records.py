def benchmark(n):
    records = []
    for i in range(10000):
        records.append({"value":i,"active":i%2==0})
    result = 0
    for trip in range(n):
        selected = [record for record in records if record["active"]]
        mapped = [record["value"]*2 for record in selected]
        result += sum(mapped)
    return result
