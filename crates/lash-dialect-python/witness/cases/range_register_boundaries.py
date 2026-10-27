def walk(start, stop, step):
    values = []
    i = 'empty'
    for i in range(start, stop, step):
        values.append(i)
        i = 'changed'
    else:
        values.append('else')
    print(values, i)

walk(5, -2, -2)
walk(0, 5, -1)
walk(5, 0, 1)
walk(9223372036854775806, 9223372036854775810, 1)
walk(-9223372036854775807, -9223372036854775811, -1)

step = -2
for i in range(5, -2, step):
    step = 0
    try:
        if i == 3:
            continue
        print(await host.echo(i))
        if i == 1:
            break
    finally:
        print('finally', i)
else:
    print('never')

for i in range(9223372036854775807, 9223372036854775810):
    print(await host.echo(i))
else:
    print('exhausted', i)

i = 0
while i < 3:
    i += 1
    if i == 2:
        continue
    print(i)
else:
    print('while', i)
