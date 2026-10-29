- `dorfq journal events|metrics|report <segment>...` reads a journal's
  segments, in the order given: `events` prints what happened as one JSON
  object per line, in the simulator's trace vocabulary where the meaning is
  the same; `metrics` prints a summary of counts, run time and latencies
  (`--json` for one JSON object); `report` prints a document in the
  simulation page's format (`--name` names it).
