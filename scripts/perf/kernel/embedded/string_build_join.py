text = ""
parts = []
for n in range(500):
    part = "row-" + str(n)
    text = text + part
    parts.append(part)
result = [text, "|".join(parts)]
