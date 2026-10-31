- A `[telemetry]` table pushes the queue's gauges and every event, as log
  records, over OTLP/HTTP as protobuf (or as JSON, with
  `encoding = "json"`) to the URLs it is given for metrics and for logs —
  one collector or two — every `interval_secs` and once more when the
  queue exits on its own, idle or fatal; a stop by signal sends nothing
  more. It needs `journal`. The GitHub forge adds two gauges: the pull
  requests labelled but blocked, and the rate limit remaining. A collector
  that cannot be reached is counted and retried, said once on stderr,
  never a reason to stop the queue; up to 10,000 events wait for it, the
  oldest dropped and counted past that.
