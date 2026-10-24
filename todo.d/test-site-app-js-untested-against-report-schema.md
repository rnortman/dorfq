Test the report page against the documents it reads. Nothing in the gate
checks that `site/app.js` parses, or that the fields it reads from a report
(`schema`, `config`, each policy's `replications` and `trace`, every event
kind's fields) exist in what `dorfq-sim run --json` writes, so a rename in
`crates/dorfq-sim/src/report.rs` or `trace.rs`, or a syntax error in
`app.js`, passes `make check` and breaks the published page. The test loads
`app.js`'s data-handling functions in a JavaScript runtime against
`//site:report_sim`'s output and asserts that the policy list and the replay
fold run without error. It needs a JavaScript runtime pinned in
`MODULE.bazel`, which is a toolchain decision for the owner: worth taking
once the page grows past its first version or becomes a production replay
tool.
The `TODO(test-site-app-js-untested-against-report-schema)` comment sits in `site/BUILD.bazel`, above the report rules.
