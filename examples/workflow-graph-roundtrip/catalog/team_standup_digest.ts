export async function team_standup_digest() {
  const messages = await slack.recent({ channel: "team-platform", since: "yesterday" });
  const activity = await github.recent({ repo: "acme/widgets", since: "yesterday" });
  const standup = await agents.spawn({
    capability: "peer",
    task: "Synthesize a concise team standup digest and call out blockers",
    seed: { slack_messages: messages, github_activity: activity },
  });
  await display.show_message({ text: standup.digest });
  return standup.digest;
}
