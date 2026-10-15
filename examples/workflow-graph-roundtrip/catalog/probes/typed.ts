type Topic = { title: string; urgent: boolean };
export async function digest(topics: Topic[]) {
  let seen = 0;
  for (const topic of topics) {
    await sleep(7000);
    if (topic.urgent) {
      await sleep(1000);
    } else {
      await sleep(2000);
    }
    seen = seen + 1;
  }
  return { seen };
}
