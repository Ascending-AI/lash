export async function research_nvidia_stock() {
  const search = await web.search({ query: "NVIDIA stock outlook" });
  const research = await agents.spawn({
    capability: "explore",
    task: "Research NVIDIA's stock outlook from the supplied web search results",
    seed: { search_results: search.results },
  });
  await display.show_message({ text: research.summary });
  return research;
}
