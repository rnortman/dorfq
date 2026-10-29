- A journaled `dorfq run` reads the journal's current segment at start, so
  the events it derives continue from what the segment recorded; a segment
  with a line that does not read stops the run at start, naming the path
  and line, before any API call.
