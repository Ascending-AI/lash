async def main(tool, output):
    def greet(name, greeting="hello", punctuation="!"):
        return f"{greeting}, {name}{punctuation}"
    output([greet("a"), greet("b", "hi"), greet("c", punctuation="?"), greet(greeting="yo", name="d")])
    log = []
    def note(value):
        log.append(value)
        return value
    def order(first, second, third=3):
        return [first, second, third]
    output([order(third=note("t"), first=note("f"), second=note("s")), log])
    def grow(item, bucket=[]):
        bucket.append(item)
        return bucket
    output([list(grow(1)), list(grow(2)), grow(3, []), grow(4)])
    return sorted(["bb", "a", "ccc"], key=lambda s: len(s), reverse=True)
