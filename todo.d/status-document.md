A live status for a queue's users. Today the only view of a real queue is
the report document `dorfq journal report` builds offline. Build one
document from the pool as it stands now plus a window of recent events,
render it to a configured GitHub issue that the queue edits in place, and
publish the same document to a branch that the simulation page reads in a
live mode. Done means the issue shows the queue, the batches in flight and
the last hours' landings and ejections within a tick of their happening,
and the page opens the published document and follows it. Deferred because
it is design work: the document's shape beside the report's, the window,
the issue's write rate against the API budget, and the page's live mode.
The `TODO(status-document)` comment sits above `document` in
`crates/dorfq/src/journal/report.rs`.
