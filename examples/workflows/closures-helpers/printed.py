#: Measure each word
async def demo(items):
    #: Describe how to measure one word
    async def size(item):
        return (len(item))
    t1 = []
    for item_1 in items:
        t1[helper_list_len(t1)] = await size(item_1)
    #: Measure every word in the list
    return t1
def main():
    demo = declared.demo
