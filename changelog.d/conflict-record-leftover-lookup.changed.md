- On recovery, the GitHub forge no longer looks for, or deletes, a build or
  probe branch beside a `conflict` batch record: two fewer ref reads per
  conflict batch per observation. The queue's own operation cannot leave
  such a branch, and like any branch no record points at, it is left alone.
