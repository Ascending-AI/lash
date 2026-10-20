#: Answer yes or no
async def demo(flag):
    #: Check whether the answer is yes
    if flag:
        #: Send back a yes
        return (await helper_py_reply((await corpus.echo("yes", result="json"))))
    else:
        pass
    #: Otherwise answer no
    return "no"
def main():
    demo = declared.demo
