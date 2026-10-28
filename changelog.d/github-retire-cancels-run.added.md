- New GitHub setting `ci = "actions"`: when the queue abandons a batch
  without landing it, it cancels the GitHub Actions runs still going on the
  batch's branch, so an abandoned batch no longer holds a runner to the end.
  It needs Actions: read/write on the queue's credential; without the
  setting, runs are left alone as before.
