- The GitHub forge queues stacked pull requests: a labelled pull request
  whose base is the branch of another queued pull request joins the queue
  on it, in stacks and trees of any depth. It is held while its head does
  not contain its base's current head, and reported conflicting with its
  base when it does not merge into it.
- A labelled pull request the queue cannot take, because the pull request
  it is based on is not queued or because it targets a branch that is
  neither trunk nor a queued pull request's, is told so in a comment,
  once for each reason, with what to do about it.
- Once a base pull request has merged into trunk, by the queue or by hand,
  the GitHub forge moves the pull requests it had queued on it onto trunk; a
  stacked pull request landed in the same batch as its base is moved onto
  trunk as its batch lands, before trunk moves, so GitHub marks it merged.
