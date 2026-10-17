tree = {"level": {"rows": [[0], [1], [2]], "counters": {"c": 0}}}
for n in range(150):
    tree["level"]["rows"][n % 3] = [n]
    tree["level"]["counters"]["c"] = tree["level"]["counters"]["c"] + 1
result = tree
