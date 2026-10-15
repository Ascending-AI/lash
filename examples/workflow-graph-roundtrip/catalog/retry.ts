export async function resilient_search() {
  const limits = [2, 50, 1];
  let failures = 0;
  for (const limit of limits) {
    try {
      const found = await web.search({ query: "workflow engines", limit });
      await display.show_message({ text: found.results[0].title });
    } catch (error) {
      failures = failures + 1;
      await display.set_status({ key: "search", value: "A search failed" });
    }
  }
  return { failures };
}
