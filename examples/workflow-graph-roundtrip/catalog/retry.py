async def resilient_search():
    failures = 0
    for limit in [2, 50, 1]:
        try:
            found = await web.search(query="workflow engines", limit=limit)
            await display.show_message(text=found["results"][0]["title"])
        except Exception:
            failures += 1
            await display.set_status(key="search", value="A search failed")
    return {"failures": failures}
