async def team_standup_digest():
    messages = await slack.recent(channel="team-platform", since="yesterday")
    activity = await github.recent(repo="acme/widgets", since="yesterday")
    standup = await agents.spawn(
        capability="peer",
        task="Synthesize a concise team standup digest and call out blockers",
        seed={"slack_messages": messages, "github_activity": activity},
    )
    await display.show_message(text=standup["digest"])
    return standup["digest"]
