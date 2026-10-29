Capture what each pull request changed when the queue first sees it.
Today the journal holds numbers, heads and holds, and nothing of a change's
content, which a model of conflict or failure rates by path would need,
and which is lost once a branch is deleted. For each `(pr, head)` first
observed, record its changed paths with additions and deletions per path,
once, keyed by pull request and head, beside the journal. Done means every
head a journal mentions has its record, and no head is fetched twice.
Deferred because it is design work: the file's format and rotation beside
the journal's, the API cost of the extra read per head, and whether paths
belong in a capture that is otherwise anonymous. The
`TODO(journal-capture-at-first-sight)` comment sits on `Writer` in
`crates/dorfq/src/journal/mod.rs`.
