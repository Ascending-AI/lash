#: Echo a value unless it is negative
async def demo(value):
    #: Check for a negative value
    if value < 0:
        #: Stop and report that it is negative
        return "negative"
    else:
        pass
    #: Echo the value
    await helper_py_reply((await tools.corpus.echo(value, result="json")))
    #: Report that it is done
    return "done"
def main():
    demo = declared.demo
