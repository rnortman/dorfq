- **Action required: every `kind = "bayes"` scheduler configuration must
  now set `notify_above`.** The scheduler can now warn an author that their
  pull request is probably broken against trunk, or probably fails only
  together with another queued pull request, without ejecting it. The
  warning is posted once per pushed head, as soon as a failed run makes the
  scheduler at least `notify_above` sure (no higher than `eject_above`; the
  examples use 0.5). On GitHub it arrives as a pull request comment; a
  warned author can rebase and fix long before the queue would eject.
- Simulator: every world table must now set `p_heed`, the chance a warned
  author rebases and pushes. The report gains how long notices take
  (`notice_p50_hours`, `notice_p95_hours`) and how many went to pull
  requests that were not broken (`notices_good`); the trace gains a
  `notice` event.
