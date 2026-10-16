async def deep_research(topic: str):
    evidence = await research.search(topic=topic, max_sources=3)
    report = await research.synthesize(topic=evidence["topic"], sources=evidence["sources"])
    await display.show_message(text=report["summary"])
    return report
