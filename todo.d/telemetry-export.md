Live telemetry from a running queue. Today the event stream and the
summary exist only offline, through `dorfq journal events` and
`dorfq journal metrics` over closed segments. First, write the event
stream live beside the journal as each tick is journaled, so a reader can
follow it without re-deriving; then push the summary's figures as gauges
and the events as log records over OTLP/HTTP with `ureq`, to an endpoint
named in the configuration. Done means a configured collector receives
both while `dorfq run` runs, and a run without the setting does neither.
Deferred because it is design work: the configuration's shape, how a
summary that is defined over a whole input becomes a gauge over a live
window, and what an unreachable collector does to the queue. The
`TODO(telemetry-export)` comment sits on the `journal` key of `Config` in
`crates/dorfq/src/config.rs`.
