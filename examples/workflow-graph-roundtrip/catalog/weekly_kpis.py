async def weekly_kpis(workspace: str):
    current = await metrics.fetch(workspace=workspace, period="current")
    previous = await metrics.fetch(workspace=current["workspace"], period="previous")
    report = await metrics.report(
        workspace=current["workspace"], current=current, previous=previous
    )
    await display.show_message(text=report["summary"])
    return report
