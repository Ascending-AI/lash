/** Pace a digest of topics */
export async function digest(topics: { urgent: boolean; }[]): Promise<{ seen: number; [key: string]: never; }> {
  /** Start counting at zero */
  let seen = 0;
  /** Go through each topic */
  for (const topic of topics) {
    /** Wait seven seconds */
    await sleep(7000);
    /** Check whether the topic is urgent */
    if (topic.urgent) {
      /** Wait one more second */
      await sleep(1000);
    }
    /** Not urgent */
    else {
      /** Wait two more seconds */
      await sleep(2000);
    }
    /** Count the topic as seen */
    seen = seen + 1;
  }
  /** Report how many topics were seen */
  return { seen };
}
