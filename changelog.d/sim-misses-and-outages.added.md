- The simulator can now model a broken trunk and CI outages. Its world
  lets CI miss a real break with probability `p_miss`, so a bad pull
  request can land and leave trunk broken until a repair is pushed
  `trunk_fix_minutes` later; and it can schedule `outages`, windows in
  which every CI run fails. **Every scenario file must now set these
  three `[world]` keys**; existing files without them are rejected.
- Simulation reports gain `landed_bad` (bad pull requests that landed)
  and `trunk_broken_hours`. A new example, `outage.toml`, takes CI down
  for twelve hours of the busy world and is included in the site's
  reports.
- A pull request waiting to re-run its own CI after a failure caused by
  trunk or an outage is now traced as `re-run N`, and the site's replay
  shows it as held rather than ready to batch.
