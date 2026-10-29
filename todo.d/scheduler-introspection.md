Record why a scheduler planned what it did. Today the journal holds each
tick's observation and plan and nothing of the scheduler's reasoning: for
`bayes`, its posteriors, the scores of the batches it weighed and the
finder it chose. Add a method to the `Scheduler` trait returning the last
step's explanation as a serialisable value (empty for schedulers with
nothing to say), journal it beside the plan, and show it in the report.
Done means a `bayes` journal answers "why was this pull request ejected"
from its own record. Deferred because it is design work: what the
explanation holds per scheduler, its size per tick against the journal's
budget, and the journal format bump it implies. The
`TODO(scheduler-introspection)` comment sits on `Scheduler::step` in
`crates/dorfq-core/src/lib.rs`.
