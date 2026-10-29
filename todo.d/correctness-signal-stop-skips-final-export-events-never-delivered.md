A stop by signal sends the telemetry exporter's pending log records. Today
`run` in `crates/dorfq/src/main.rs` handles no signal: SIGTERM and SIGINT
end the process with the default action, so `Exporter`'s `Drop`, the
final export, never runs. Every `systemctl stop` or `restart`, and so
every `make local-deploy`, loses up to `interval_secs` of log records (more
while the collector is failing): they are in the events file, are not
counted in `dorfq_telemetry_events_dropped_total`, and the next start
replays them through the journal's seed, which never reaches the
exporter. Done means a SIGTERM or SIGINT makes `run` return normally after
the tick in progress, so the exporter's drop delivers what is pending, and
a CLI test that signals a running queue with a collector sees the events
file's lines arrive. Deferred because it is design work: the GitHub
forge's `wait` sleeps until its next deadline and must be woken early,
which needs a pinned signal-handling dependency or a self-pipe; and the
exit export can then outlast a supervisor's stop timeout (systemd's 90 s)
against a slow collector with a full buffer, which the stop has to settle.
The `TODO(correctness-signal-stop-skips-final-export-events-never-delivered)`
comment sits on the run loop's `wait` in `crates/dorfq/src/main.rs`.
