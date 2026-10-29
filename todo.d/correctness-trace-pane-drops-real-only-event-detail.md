The report page's trace pane prints only the bare kind for the events a
`dorfq journal report` carries and a simulation does not: `error`,
`restart`, `conflict`, `conflicting`, `held` and `trunk` fall through to
`eventText`'s `default` arm in `site/app.js`. A failed tick shows as the word
`error` without its message or whether it was fatal; a conflicted build
without its batch, pull request and what it conflicted with; a hold or a
trunk move without its pull request or position. Give each kind a text line
built from its fields, as named by `Kind` in
`crates/dorfq/src/journal/events.rs` — for example
`error: <message> (fatal)`, `conflict <batch>: <pr> with <with>`,
`conflicting <pr> with <with>`, `held <pr>`,
`trunk <position> <sha>, landed <prs>, <n> foreign`,
`restart dorfq <version>`. Done when a
journal report's trace pane names, for each of these kinds, the fields an
operator diagnosing a stall reads.
