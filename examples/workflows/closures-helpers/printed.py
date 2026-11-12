#: Measure each word
async def demo(items: list[str]) -> list[int]:
    #: Describe how to measure one word
    @callable.direct
    def size(item):
        return len(item)
    #: Measure every word in the list
    return [(callable.invoke(size, item)) for item in items]
