export async function weekly_kpis(workspace: string) {
  const current = await metrics.fetch({ workspace, period: "current" });
  const previous = await metrics.fetch({ workspace: current.workspace, period: "previous" });
  const report = await metrics.report({ workspace: current.workspace, current, previous });
  await display.show_message({ text: report.summary });
  return report;
}
