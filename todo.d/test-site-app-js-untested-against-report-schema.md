Test the report page against the documents it reads. Nothing in the gate
checks that `site/app.js` parses, or that the fields it reads from a report
(`schema`, `config`, each policy's `replications` and `trace`, every event
kind's fields, every metric key in `ROWS`) exist in what `dorfq-sim run
--json` writes, so a rename in `crates/dorfq-sim/src/report.rs`,
`metrics.rs` or `trace.rs`, or a syntax error in `app.js`, passes
`make check` and breaks the published page; a metric added to `Metrics` gets
no table row and no glossary definition. The test loads `app.js`'s
data-handling functions in a JavaScript runtime against
`//site:report_sim`'s output and asserts that the policy list and the replay
fold run without error and that `ROWS` names exactly the metric keys of
each policy's replications. It needs a JavaScript runtime pinned in
`MODULE.bazel`, which is a toolchain decision for the owner: worth taking
once the page grows past its first version or becomes a production replay
tool.
The `TODO(test-site-app-js-untested-against-report-schema)` comment sits in `site/BUILD.bazel`, above the report rules.
