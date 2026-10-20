#: Echo two values at once
async def demo():
    "use kernel"
    #: Describe how to echo one value
    async def work(value):
        return (await helper_py_reply((await corpus.echo(value, result="json"))))
    #: Echo "a" and "b" side by side and wait for both
    with k.return_():
        with k.bind("t1"):
            work
            with k.bind("t2"):
                "a"
                with k.bind("t3"):
                    t2
                    with k.bind("t4"):
                        work
                        with k.bind("t5"):
                            "b"
                            with k.bind("t6"):
                                t5
                                with k.bind("t7"):
                                    with k.list():
                                        with k.spawn(helper_py_task_run):
                                            with k.closure([]):
                                                with k.block():
                                                    return (await t1(t3))
                                        with k.spawn(helper_py_task_run):
                                            with k.closure([]):
                                                with k.block():
                                                    return (await t4(t6))
                                    (await helper_py_gather(t7))
def main():
    "use kernel"
    demo = declared.demo
