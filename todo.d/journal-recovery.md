Startup recovery from the tick journal. Today a restart builds the
scheduler fresh and the journal is only appended to. Before the first live
tick, step the current segment's `report` observations through the fresh
scheduler, in order, and stop at the first plan this build makes
differently from the journaled one; the replay function in the `journal`
module's tests (`crates/dorfq/src/journal/tests/harness.rs`) is the one to
promote. Two preconditions. First, the GitHub forge's trunk position must
continue across a restart: today `Counted` in
`crates/dorfq-forge-github/src/lib.rs` starts at 0 at whatever trunk is
current, and its sha-to-position history and covered map are lost with the
process, so a replayed scheduler's batch origins would sit above the live
position and read a trunk that went backwards. The forge must resume its
count from a persisted trunk sha and position, or derive them from the
journal's last observation plus one `compare`. Second, the header's
`scheduler` table is stored as written and read by the offline tools as an
untyped table; recovery parses it into `SchedulerConfig`, which rejects
unknown keys, so a journal can be replayed only by a build whose scheduler
configuration still accepts the settings it was written with — a segment
written before a setting was renamed or retired is readable but not
replayable.
Done means the replay-equivalence test's journal, replayed at start against
a forge resumed at its last position, plans as the scheduler that wrote it.
Deferred because the forge's position continuity is design work of its
own. The `TODO(journal-recovery)` comment sits in `run` in
`crates/dorfq/src/main.rs`, where the scheduler is built.
