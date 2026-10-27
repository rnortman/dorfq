The `bayes` score pays `p_node · p · V` for a landing that may not happen,
in two ways that are one problem. First, under `speculation = "chain"` a
culprit finder is scored as `p·V + (1 − p)·R − run_cost` with `V` its
members' landing value, as though its pass lands: `chain_plan` in
`crates/dorfq-scheduler/src/bayes/chain.rs` scores every finder at
`p_node = 1`, and `score` in `bayes/policy.rs` sums the members' values into
the landing term. Under load a passed finder does not land: `chain_plan`
retires it whenever a chain batch holds trunk, so `p·V` prices a landing
that will not happen, and the score has no term for what the pass actually
does — discount its suspects by `p_miss` in the failed runs they share with
the culprit, and cover them against a second proposal at that trunk
position. A finder that is a strict prefix of an `Atomic`
unit never lands under `chain` at any load — `split_atomic` retires its pass
— and is priced the same; a quiet queue in the simulator's `sim.toml` re-ran
one such finder, scored on a landing it could not make, for 280 hours before
`suspects` was bounded to one proposal of a set per trunk position. On one 12/hr sweep point traced whole, finders
that passed and were not landed took 44% of the chain's run-hours, up from
31% before the walk that proposes more of them (their count nearly doubled);
they win lanes and members over chain extensions, so the median wait rises
with every rule that proposes more of them and with `w_age`, which grows
`V`. Over the sweep's 12/hr cells measured — each CI profile at `w_age` 0
and 0.02, and pooled over `w_age` — that walk's trade is +15–24% correct
ejections and 7–19% faster detection against a median wait up 3–16% at
`w_age = 0` (1–10% at 0.02) and chain runner-hours up 7–9%. Second, under
both speculations a batch on a node — the chain's end, or a tree node — is
priced as the product of its own chance of passing and the chance the node
stands: `p_end` in `chain.rs`, `Node::weight` and a node's `p` in
`bayes/tree.rs`, and `p_node · p` in `score`. Each factor is the joint
posterior within one batch, but the product across batches treats them as
independent, and batches whose trees share a failed run are not: `[10, 11]`
on a pending `[8, 9]`, right after `[8, 9, 10, 11]` failed, is priced at
about a quarter where the joint is near zero, so a batch that cannot pass is
started and later cancelled or fails. In the sweep's 12/hr traces such
starts cost about 1–2% of lane-hours. Design, once for both: what a batch's
pass is worth under each speculation — the landing it makes if it can land,
and what it teaches if it cannot — with the node factor as the joint over
the union of the path's and the batch's exercised causes and the flake
factor once per pending run, which `Inference::p_none` can give; the
resolution term and a tree node's sibling terms need their own joint
(`P(node stands ∧ this fails) = P(path) − P(path ∪ this)`, and
inclusion–exclusion over siblings). Measure on the sweep's 12/hr points
against `land_p50`, `runner_hours`, `detect_p50` and `ejected_bad`. The
`TODO(bayes-landing-term)` comments sit at `score` in `bayes/policy.rs`, at
the finder scoring and at `p_end` in `bayes/chain.rs`, and at `Node::weight`
in `bayes/tree.rs`.
