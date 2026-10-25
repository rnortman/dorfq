When `bayes` singles out a pair of pull requests while both are still
queued, it ejects the newer at once with `InteractsWith`. Neither is broken
against trunk until the other lands, so the simulator counts that ejection
as `ejected_good`. Land the older first and eject the newer only after, or
keep the two apart until then; the design question is what holds the newer
in the meantime. The `TODO(bayes-pair-eject-timing)` comment is on
`culprit` in `crates/dorfq-scheduler/src/bayes/policy.rs`.
