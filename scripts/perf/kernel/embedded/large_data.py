groups = {}
total = 0
evens = []
odds = []
for item in range(512):
    key = "bucket_" + str(item % 16)
    groups[key] = groups.get(key, 0) + 1
    total = total + item
    if item % 2 == 0:
        evens.append(item)
    else:
        odds.append(item)
lines = []
for key in groups:
    lines.append(key + ":" + str(groups[key]))
result = {"count": 512, "total": total, "groups": groups, "evens": len(evens), "odds": len(odds), "summary": "|".join(lines)}
