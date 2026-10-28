async def compute():
    a = -0.0
    b = 0.5
    for i in range(3):
        a += b * b
        b += 1
    print(a, b)
    negative = -0.0
    print(negative * 1.0, negative + -0.0, -negative)
    infinite = 1e309
    other = -1e309
    print(infinite * 2.0, other + 1.0, infinite - infinite)
    mixed = 0.5
    mixed *= 2
    print(mixed + 9007199254740993)
    try:
        print(a / 0.0)
    except ZeroDivisionError:
        print('zero division')
    print(await host.echo(str(a)))
    return a + b
print(await compute())

value = 0.5
for i in range(2):
    if i == 1:
        value = 'a'
    value += value
print(value)

def outer():
    x = 0.5
    def change():
        nonlocal x
        x = 'a'
    change()
    x += 'b'
    print(x)
outer()

def maybe(flag):
    if flag:
        x = 0.5
    try:
        x += 1.0
        print(x)
    except UnboundLocalError:
        print('unbound')
maybe(False)
maybe(True)

async def accumulate():
    values = [1, 2, 3]
    alias = values
    total = 0
    for i in range(3):
        total += values[i]
        if i == 0:
            alias[1] = 0.5
        if i == 1:
            alias[2] = await host.echo(4)
    print(total)
    alias[0] = 'bad'
    try:
        total += values[0]
    except TypeError as error:
        print(type(error).__name__, str(error))
    return total
print(await accumulate())

counter = -2
before = 99
after = 99
while counter < 3:
    before = counter
    counter += 1
    after = counter
print(await host.echo(str((counter, before, after))))

counter = 9223372036854775806
before = 0
after = 0
while counter < 9223372036854775807:
    before = counter
    counter += 2
    after = counter
print(await host.echo(str((counter, before, after))))

counter = 0.0
before = -1.0
after = -1.0
while counter < 0.5:
    before = counter
    counter += 0.1
    after = counter
print(counter, before, after)
