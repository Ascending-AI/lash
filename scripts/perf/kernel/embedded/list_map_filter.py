source = []
for n in range(800):
    source.append(n)
doubled = [item * 2 for item in source]
result = [item for item in doubled if item % 3 == 0]
