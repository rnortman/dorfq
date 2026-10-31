The `dorfq journal` readers hold their whole input in memory: `journal::read`
(`crates/dorfq/src/journal/mod.rs`) collects every entry of every segment
into one `Vec<Entry>`; `events::derive` takes that slice and returns a
`Vec<Event>`; `summary::summarise` takes the event slice; and
`dorfq journal events` builds its whole output before writing it. Memory
therefore grows with the length of the capture rather than with one line.
The pieces to stream are in place: `journal::entries` yields one segment's
entries line by line from a buffered reader, and the event fold is public
and per-entry (`Fold::entry`, `take`, `finish`), with state bounded by the
last two observations; the queue's start-up read of its current segment
uses both. What remains is the readers consuming them without collecting:
`read` and its three commands taking entries per segment as they come,
`journal events` writing each event as it is derived, the summary
consuming events as they come, and `journal report` keeping the first
header as it passes. Done when `dorfq journal metrics` over many segments
holds at most one line's entry and the fold's state at a time, with the
existing reader, event and summary tests unchanged in what they assert. The
`TODO(efficiency-journal-readers-materialise-every-segment)` comment sits on
the collecting loop in `read_segment`.
