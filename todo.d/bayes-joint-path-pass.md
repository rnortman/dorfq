The `bayes` scheduler prices a batch on a node — the chain's end, or a tree
node — as the product of its own chance of passing and the chance the node
stands: `p_end` in `crates/dorfq-scheduler/src/bayes/chain.rs`,
`Node::weight` and a node's `p` in `bayes/tree.rs`, and `p_node · p` in
`score` in `bayes/policy.rs`. Each factor is the joint posterior within one
batch, but the product across batches treats them as independent, and
batches whose trees share a failed run are not: `[10, 11]` on a pending
`[8, 9]`, right after `[8, 9, 10, 11]` failed, is priced at about a quarter
where the joint is near zero, so a batch that cannot pass is started and
later cancelled or fails. In the sweep's 12/hr traces such starts cost about
1–2% of lane-hours. The right landing term is the joint over the union of
the path's and the batch's exercised causes, with the flake factor once per
pending run, which `Inference::p_none` can give; the resolution term and a
tree node's sibling terms need their own joint
(`P(node stands ∧ this fails) = P(path) − P(path ∪ this)`, and
inclusion–exclusion over siblings). Design: the score's node factor under
both speculations. Measure on the sweep's 12/hr point.
