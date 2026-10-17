async def demo():
    try:
        async def work(value):
            with k.return_():
                with k.bind("t1"):
                    with k.choose():
                        value is k.absent
                        (await helper_py_arg_missing("work"))
                        None
                    (await helper_py_reply((await tools.corpus.echo(value))))
        with k.return_():
            with k.bind("t2"):
                work
                with k.bind("t3"):
                    "a"
                    with k.bind("t4"):
                        t3
                        with k.bind("t5"):
                            work
                            with k.bind("t6"):
                                "b"
                                with k.bind("t7"):
                                    t6
                                    with k.bind("t8"):
                                        with k.list():
                                            with k.spawn(helper_py_task_run):
                                                with k.closure([]):
                                                    with k.block():
                                                        return (await t2(t4))
                                            with k.spawn(helper_py_task_run):
                                                with k.closure([]):
                                                    with k.block():
                                                        return (await t5(t7))
                                        (await helper_py_gather(t8))
    finally:
        await helper_tasks_cancel_all()
def main():
    demo = declared.demo
