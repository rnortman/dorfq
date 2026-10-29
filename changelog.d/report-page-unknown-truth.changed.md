- The simulation page renders a report without truth, such as one from
  `dorfq journal report`: a pull request's truth reads "unknown", an
  ejection is neither justified nor innocent and counts under a new
  `ejected` counter beside bad and good, the trace prints an ejection's or
  notice's reason without a justification word ("reason unknown" when the
  journal lacks it), a failed batch's tooltip names its failed checks, and
  pull requests merged outside the queue or lost while it was down depart
  as such, with a `merged outside` counter. Simulator reports render as
  before.
