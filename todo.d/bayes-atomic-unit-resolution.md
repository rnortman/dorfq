An `Atomic` unit whose runs all fail blames every member equally under
`bayes`: each member's `Bad` cause is exercised by every run of the unit,
so their posteriors stay equal and `singled_out` never fires; only closure
finders of sub-prefixes can separate them, and whether one runs depends on
its score. `bisect` ejects the unit's `Atomic` members on its first own
failure instead. Decide between one cause per unit in the model and a rule
for a unit that fails alone, measured on a broken member of a unit of
seven in the simulator. Model design. The `TODO(bayes-atomic-unit-resolution)`
comment is on `singled_out` in `crates/dorfq-scheduler/src/bayes/policy.rs`.
