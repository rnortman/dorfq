Publish the live status document for the simulation page. The queue keeps
a `Document` (the pool now, when each pull request and batch entered its
state, and a 24-hour window of events and figures) and renders it to the
status issue; nothing publishes it as data. Write the document as JSON to a
branch the queue owns, and give the simulation page a live mode that opens
the published document and follows it. Done means the page, pointed at a
running queue's repository, shows the same queue, batches and window the
status issue does and updates as the queue writes. Deferred because it
waits on decisions: where the page is hosted, how a browser reads a private
repository without a token of its own, whether the document lives on a
queue-owned ref under `refs/dorfq/`, and which JavaScript test toolchain the
build brings in. The `TODO(status-document)` comment sits above `Document`
in `crates/dorfq/src/journal/status.rs`.
