- The GitHub forge lists every open pull request on each tick, not only
  those against trunk, and may change a queued pull request's base to trunk
  once the queued pull request it was based on has merged into trunk. This
  needs no new permission: changing a base is Pull requests: write, which
  the queue already requires.
- A pull request that leaves the queue is told "Left the merge queue (label
  removed, pull request closed, or retargeted)".
