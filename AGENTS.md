# Agents

- Releases: follow `.claude/skills/release/SKILL.md`. It covers the version
  bump, the tag, the release workflow, and moving DALP to the new version.
- Old and new versions share one state directory, so data formats may only
  grow. See "Mixed versions" in `README.md` before you change `queue::Entry`,
  `machine::MachineSample`, the database schema, or job keys.
