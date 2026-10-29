A CLI test of a fatal tick with `[telemetry]` configured, after ticks that
observed: the final export arrives before the exit code 1, and the fatal
tick hands the exporter no gauges and does not count as observed
(`dorfq_tick_age_seconds` measures from the tick before it). Today the
mock forge writes its author messages to the same stdout as the reports,
so the only way the CLI tests make a tick fatal, a closed stdout, fails
the first tick, before anything observed or any event is pending, and a
test over it cannot fail on any of these promises. Deferred because it
needs a decision on how a test reaches a permanent error after observing:
a mock scenario key that fails a given tick, a separate sink for the
mock's messages, or a test target over `run`'s tick construction. The
`TODO(test-missing-fatal-tick-telemetry-wiring)` comment sits on
`a_fatal_tick_is_journaled_before_the_exit` in `crates/dorfq/tests/cli.rs`.
