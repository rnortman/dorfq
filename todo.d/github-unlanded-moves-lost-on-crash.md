The GitHub forge moves each stacked member of a batch onto trunk just before
it fast-forwards trunk, because GitHub refuses the move once trunk contains
the member's head. When the batch then does not land, `fast_forward` in
`crates/dorfq-forge-github/src/lib.rs` puts each moved member back on its
base branch. A process that dies after a move and before the fast-forward
cannot do that: the base branches it moved members off live only in memory,
so the restarted queue reads those members as targeting trunk. If their base
pull requests then do not land with them, nothing stops the queue from
landing a child alone, which lands its base's commits with it. Done means:
the `landing` state of the batch record also names each stacked member's base
branch, and recovering a `landing` record whose tip is not in trunk moves each
such member still open on trunk back onto that branch before the observation
is reported, with a test that restarts the forge between the moves and the
fast-forward. The `TODO(github-unlanded-moves-lost-on-crash)` comment sits
above the moves. Deferred because it changes the batch record's grammar,
which the queue's recovery depends on, to cover a window of a few API calls.
Whether that is worth doing is for the owner and a design cycle to decide.
