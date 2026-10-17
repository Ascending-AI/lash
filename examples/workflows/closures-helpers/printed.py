async def demo(items):
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
    return t2
def main():
    demo = declared.demo
