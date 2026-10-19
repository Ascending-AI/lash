#: Measure each word
async def demo(items):
    #: Describe how to measure one word
    async def size(item):
        with k.return_():
            with k.bind("t1"):
                with k.choose():
                    item is k.absent
                    (await helper_py_arg_missing("size"))
                    None
                (len(item))
    t2 = []
    for item_1 in items:
        t2[helper_list_len(t2)] = await size(item_1)
    #: Measure every word in the list
    return t2
def main():
    demo = declared.demo
