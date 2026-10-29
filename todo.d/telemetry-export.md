Push the queue's telemetry from a running `dorfq run`: the summary's
figures as gauges and the events as log records over OTLP/HTTP with
`ureq`, to an endpoint named in the configuration. The event stream is
already derived live and written to the events file (`events` in the
configuration); what remains is the export. Delivery rule: a configuration
that names an unusable endpoint fails at load; a collector that cannot be
reached is counted and retried, never a reason to stop the queue. Two
gauges are the forge's own, in no observation and so in no journal, and
need a way out of it: the pull requests labelled but not enqueued (the
GitHub forge's `blocked` set) and the last rate-limit remaining GitHub
reported. A gauge over a live
window takes the samples and counts whose end falls in the trailing window
of forge time: a lifetime or run that began before the window and ended in
it is in, one that ended before is out, one still open is in no figure.
Done means a configured collector receives both while `dorfq run` runs,
and a run without the setting does neither. Deferred because it is design
work: the configuration's shape, the window lengths, and how a
forge-private gauge leaves the forge. The `TODO(telemetry-export)` comment
sits on the `events` key of `Config` in `crates/dorfq/src/config.rs`.
