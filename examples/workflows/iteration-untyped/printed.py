#: Echo every item
async def demo(items):
    #: Start an empty list of replies
    out = []
    #: Go through each item
    for item in items:
        #: Echo the item and keep the reply
        out[helper_list_len(out)] = (await helper_py_reply((await corpus.echo(item, result="json"))))
    #: Give back the replies
    return out
def main():
    demo = declared.demo
