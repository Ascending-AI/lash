def benchmark(n):
    result = 0
    for trip in range(n):
        text = ""
        for i in range(100):
            text += f"Tool {i}: value={i*3}; status=ready\n"
        result += len(text)
    return result
