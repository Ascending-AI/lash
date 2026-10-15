export async function deep_research(topic: string) {
  const evidence = await research.search({ topic, max_sources: 3 });
  const report = await research.synthesize({ topic: evidence.topic, sources: evidence.sources });
  await display.show_message({ text: report.summary });
  return report;
}
