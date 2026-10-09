async def main(tool, output):
    shared = [1]
    alias = shared
    record = {"items": shared}
    got = await tool("echo", "first")
    alias.append(got)
    output(shared)
    output([alias is shared, record["items"] is shared])
    record["items"].append(await tool("echo", "second"))
    output(shared)
    copy = list(shared)
    copy.append("only the copy")
    output([shared == copy, shared is not copy, len(shared), len(copy)])
    return shared
