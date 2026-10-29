- On GitHub, the queue no longer lists every open pull request in the
  repository on each observation. It lists only the pull requests carrying
  the queue label, which GitHub filters on its side, and reads each of them
  on its own. A pull request based on a branch that no queued pull request
  heads costs one more lookup, for the open pull request with that head
  branch. A label containing a comma is now rejected at startup, since
  GitHub's label filter cannot name it.
- A pull request the label listing misses, as paging can when a pull request
  opens or closes between two page reads, is now read on its own in the same
  observation, instead of failing the observation and waiting for the next.
