# PY-FSTRING: typed conversion preserves Python spelling and captures each field before later effects.
small = -7
large = 2 ** 100
text = "é😀"
print(f"{small}|{large}|{-large}|{text}|{True}|{False}|{None}")
print(f"{small!s}|{small!r}|{text!r}|{text!a}|{-0.0}|{small:}")
print(f"{-9223372036854775808}|{9223372036854775807}|{9223372036854775808}")
joined = f"{text}:{large}:{text}"
print(len(joined), joined)

def str(value):
    return "shadow"

print(f"{small}", str(small))

current = "first"
def change():
    global current
    current = "second"
    return 3

print(f"{current}:{change()}:{current}")

async def suspended():
    global current
    current = "third"
    return await host.echo(11)

print(f"{current}:{await suspended()}:{current}")
print(len(joined))

def build():
    for trip in range(1):
        value = ""
        for i in range(3):
            value += f"Tool {i}: value={i*3}; status=ready\n"
            if i == 1:
                value += value
        print(value)
build()
