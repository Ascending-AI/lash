/** Search the web, counting failures */
export async function resilient_search(): Promise<{ failures: number }> {
  /** Choose the result limits to try */
  const limits = [2, 50, 1];
  /** Start counting failures at zero */
  let failures = 0;
  /** Try each limit */
  for (const limit of limits) {
    /** Search and show the top result */
    try {
      /** Search the web for workflow engines */
      const found = await web.search({ query: "workflow engines", limit });
      /** Show the top result's title */
      await display.showMessage({ text: found.results[0].title });
    }
    /** If the search fails */
    catch (error) {
      /** Count the failure */
      failures = failures + 1;
      /** Say that a search failed */
      await display.setStatus({ key: "search", value: "A search failed" });
    }
  }
  /** Report how many searches failed */
  return { failures };
}
