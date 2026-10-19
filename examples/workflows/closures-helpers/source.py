#: Measure each word
async def demo(items: list[str]):
    #: Describe how to measure one word
    def size(item):
        return len(item)
    #: Measure every word in the list
    return [size(item) for item in items]
