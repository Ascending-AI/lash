# Host development skills

A skill is a task guide that a host developer or their agent can follow while
building on lash. Read the relevant skill, then open its linked documentation
and tested examples. Contributor and repository guidance stays in
[docs/agents/](../docs/agents/).

| Name | When to use it |
| --- | --- |
| [making-packages](making-packages/SKILL.md) | Add or release a host library package that guest TypeScript or Python code calls, including its operation runners and workflow attachment. |

Each skill uses the Agent Skills format: a directory named after the skill,
containing `SKILL.md`. Optional `references/` files hold focused details needed
for the task. For example, the package skill's
[surface reference](making-packages/references/surface.md) explains the parser's
less obvious rules.

Format and maintenance rules:

- Start `SKILL.md` with YAML frontmatter containing `name` and `description`.
  The name matches the directory, using lowercase letters, digits and hyphens.
  The description says when the skill applies.
- Keep the main procedure short (aim below 300 lines). Link to existing docs
  and tested files instead of copying their implementations.
- Keep copied code blocks to about five lines. Surface syntax examples must
  parse: quote fixture lines or check them through the real parser.
- Use repository-relative file links. Confirm every cited path and symbol
  exists when changing a skill. Each derived reference names its source of truth.
- Add a row here for each skill. Add supporting files only when the task needs
  information the linked docs do not already give compactly.
