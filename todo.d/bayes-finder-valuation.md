The `bayes` chain scores a culprit finder as `p·V + (1 − p)·R − run_cost`
with `V` its members' landing value, as though its pass lands: `chain_plan`
in `crates/dorfq-scheduler/src/bayes/chain.rs` scores every finder at
`p_node = 1`, and `score` in `bayes/policy.rs` sums the members' values
into the landing term. Under load a passed finder does not land: `chain_plan`
retires it whenever a chain batch holds trunk, so `p·V` prices a landing
that will not happen, and the score has no term for what the pass actually
does — clear its suspects for the chain and strip them from the failed runs
they share with the culprit. On one 12/hr sweep point traced whole, finders
that passed and were not landed took 35% and 44% of the two chains'
run-hours, up from 21% and 31% before the walk that proposes more of them
(their count doubled); they win lanes and members over chain extensions, so
the median wait rises with every rule that proposes more of them and with
`w_age`, which grows `V`. Over the sweep's 12/hr cells measured — each CI
profile at `w_age` 0 and 0.02, and pooled over `w_age` — that walk's trade
is +15–24% correct ejections and 7–19% faster detection against a median
wait up 3–16% at `w_age = 0` (1–10% at 0.02) and full-chain runner-hours
up 7–9%. This is `bayes-joint-path-pass` seen from the other side: there
the landing value is multiplied by a node factor that treats a path of
pending batches as passing independently, here it is credited to a finder
whose pass does not land; in both the score pays `p_node · p · V` for a
landing that may not happen, and the landing term should be designed once,
not patched twice. Design: what a finder's pass is worth under each
speculation — the value of what it teaches, not the landing it does not
make — and whether the tree, where a finder on a node can land, needs the
same term. Measure on the sweep's 12/hr points against `land_p50`,
`runner_hours`, `detect_p50` and `ejected_bad`.
