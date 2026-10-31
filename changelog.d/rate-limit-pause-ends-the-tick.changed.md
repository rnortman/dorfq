- When GitHub answers a comment post (or, with a status issue, a status edit)
  with a rate-limit pause, the queue now writes nothing until the pause ends:
  no merge, branch update, label removal or comment. A tick whose plan had a
  write to make is journaled with what it observed and planned and a
  transient error. Before, a rate-limited comment post left the tick running.
  A notice the tick owed is kept and posted when the pause ends. A landing
  interrupted by a pause is finished at the next tick, so a stacked pull
  request it had moved onto trunk stays there until then.
