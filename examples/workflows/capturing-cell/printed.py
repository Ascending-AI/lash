async def main():
    limit = 3
    shared = [1]
    async def add(x):
        with k.return_():
            with k.bind("t1"):
                with k.choose():
                    x is k.absent
                    (await helper_py_arg_missing("add"))
                    None
                (await helper_py_add((await helper_py_add(x, limit)), (await helper_py_getitem(shared, 0))))
    with k.do():
        with k.bind("t2"):
            [1, add]
            with k.bind("t3"):
                record({"list": t2})
                (await helper_py_reply((await corpus.capture(t3, result="json"))))
    await helper_py_setitem(shared, 0, 9)
