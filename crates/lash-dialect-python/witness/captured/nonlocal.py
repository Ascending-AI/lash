async def main(tool, output):
    def make_counter(start):
        count = start
        def bump(step=1):
            nonlocal count
            count += step
            return count
        def read():
            return count
        return bump, read
    bump, read = make_counter(10)
    bump()
    bump(5)
    output(read())
    other, other_read = make_counter(0)
    other()
    output([read(), other_read()])
    adders = [lambda x, n=n: x + n for n in range(3)]
    late = [lambda: i for i in range(3)]
    return [[f(10) for f in adders], [f() for f in late]]
