- The tick journal: with `journal = "<path>"` at the top of the
  configuration, `dorfq run` appends every tick's observation and plan to
  that file as one JSON line, a quiet tick as a short `same` line and a
  failed tick with its error. A header line opens each process's lines. The
  file is closed as `<path>.<opened>` when the queue goes idle after work,
  or past 64 MiB, and a fresh one opened; closed segments are never deleted.
  A file at the path that is not a journal stops the run at start.
- A tick whose plan a forge error cut short is printed to stdout like any
  other tick, under the same rule for repeated ticks, before the stderr line
  naming the error: the plan partly ran, and the line shows what it was.
