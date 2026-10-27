# Match the TypeScript row: parse, update and compactly encode 128 records.
import json


def benchmark(n):
    records = []
    for i in range(128):
        records.append({"id": i, "name": "tool-record-" + str(i), "value": i, "active": i % 2 == 0})
    source = json.dumps({"tool": "search", "records": records}, separators=(",", ":"))
    result = 0
    for trip in range(n):
        data = json.loads(source)
        for record in data["records"]:
            record["value"] += 1
        result += len(json.dumps(data, separators=(",", ":")))
    return result
