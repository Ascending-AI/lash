kept = []
for n in range(400):
    scratch = {"index": n, "pair": [n, n + 1], "label": "row-" + str(n)}
    if n % 40 == 0:
        kept.append(scratch)
result = kept
