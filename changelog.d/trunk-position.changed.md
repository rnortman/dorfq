- Pushes to trunk that bypass the queue, such as an admin push or a merge
  without the queue label, are now taken into account. Previously the `bayes`
  scheduler kept its own count of how far trunk had moved, so it did not see
  these pushes. A pull request broken by such a push could then be retried in
  batch after batch, and an innocent batch-mate could be ejected in its place.
  The forge now reports trunk's position, counted in pull requests, and the
  scheduler uses that number. A pull request broken this way is ejected for
  its own failed batch.
- On GitHub, a pull request's `behind` count (how far trunk has moved past
  what its CI tested) now counts the queue's landings by pull request, not by
  commit. The forge also makes far fewer API calls. It makes one `compare`
  call for each new head and for each trunk move the queue did not make.
  Before, it made one per enqueued pull request on every tick.
