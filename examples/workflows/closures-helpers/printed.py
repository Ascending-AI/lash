#: Measure each word
async def demo(items: list[str]):
    #: Describe how to measure one word
    def size(item):
        return len(item)
    t1 = []
    for item_1 in items:
        t1.append(size(item_1))
    #: Measure every word in the list
    return t1
