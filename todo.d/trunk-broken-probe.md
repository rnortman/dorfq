Neither `Bisect` nor `Bayes` notices that trunk itself is broken. A flaky
pass can land a bad pull request; after it, every batch built on trunk
fails, `Bisect` ejects each singleton in turn and `Bayes` singles out each
finder's suspect, so innocent pull requests are ejected one after another.
A probe on trunk (a `BatchStart` with no members, which the contract allows)
after some number of consecutive own-failures would tell. When to probe,
what to hold while trunk is broken, how to tell the operator and when to
resume on a passing probe are design work not yet done. The
`TODO(trunk-broken-probe)` comments sit at `Bisect`'s singleton ejection in
`crates/dorfq-scheduler/src/bisect.rs` and at `Bayes`'s finder generation in
`crates/dorfq-scheduler/src/bayes/policy.rs`.
