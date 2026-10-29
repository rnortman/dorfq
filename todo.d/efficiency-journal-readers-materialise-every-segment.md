The `dorfq journal` readers hold their whole input in memory: `journal::read`
(`crates/dorfq/src/journal/mod.rs`) loads each segment whole and collects
every entry of every segment into one `Vec<Entry>`; `events::derive` takes
that slice and returns a `Vec<Event>`; `summary::summarise` takes the event
slice; and `dorfq journal events` builds its whole output before writing it.
Memory therefore grows with the length of the capture rather than with one
line. Make the read path stream: the reader yields entries line by line from
a buffered reader, one segment at a time; the event fold takes entries one at
a time (a public fold with `push` and `finish`, or `derive` over an iterator)
and the summary consumes events the same way; `journal events` writes each
event as it is derived; `journal report` keeps the first header as it passes.
The API shape is for the first caller that reads a long capture to settle
(the live events file, a status job over a window, fitting over the whole
capture). Done when `dorfq journal metrics` over many segments holds at most
one line's entry and the fold's state at a time, with the existing reader,
event and summary tests unchanged in what they assert.
