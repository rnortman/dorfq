- The events file: with `events = "<path>"` beside `journal`, `dorfq run`
  writes the event stream as it runs, one JSON line per event as
  `dorfq journal events` prints it, and rotates the file with the journal
  as `<path>.<opened>`. `events` without `journal` is refused at load, and
  an `events` path that is the journal's own file stops the run at start.
