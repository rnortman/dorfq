//! Required-check evaluation: the results on one commit, reduced to a
//! [`CiStatus`] by the check names the configuration requires.

use dorfq_core::CiStatus;

use crate::api::{CheckOutcome, CheckResult};

/// The status of `required` among `results`, which are newest first.
///
/// Each required name is decided by its newest result, so a re-run
/// supersedes the run it repeats; a name with no result is pending. Any
/// required name failed makes the whole `Failed`, since the commit cannot
/// pass whatever is still running; otherwise any pending makes it
/// `Pending`. Results under names that are not required are ignored.
pub fn evaluate(required: &[String], results: &[CheckResult]) -> CiStatus {
    let mut pending = false;
    for name in required {
        match results.iter().find(|r| &r.name == name).map(|r| r.outcome) {
            Some(CheckOutcome::Failed) => return CiStatus::Failed,
            Some(CheckOutcome::Passed) => {}
            Some(CheckOutcome::Pending) | None => pending = true,
        }
    }
    if pending {
        CiStatus::Pending
    } else {
        CiStatus::Passed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(name: &str, outcome: CheckOutcome) -> CheckResult {
        CheckResult {
            name: name.to_owned(),
            outcome,
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn all_required_passed_is_passed() {
        let results = [
            result("build", CheckOutcome::Passed),
            result("test", CheckOutcome::Passed),
        ];
        assert_eq!(
            evaluate(&names(&["build", "test"]), &results),
            CiStatus::Passed
        );
    }

    #[test]
    fn a_missing_required_check_is_pending() {
        let results = [result("build", CheckOutcome::Passed)];
        assert_eq!(
            evaluate(&names(&["build", "test"]), &results),
            CiStatus::Pending
        );
        assert_eq!(evaluate(&names(&["build"]), &[]), CiStatus::Pending);
    }

    #[test]
    fn one_failed_and_one_running_is_failed() {
        let results = [
            result("build", CheckOutcome::Pending),
            result("test", CheckOutcome::Failed),
        ];
        assert_eq!(
            evaluate(&names(&["build", "test"]), &results),
            CiStatus::Failed
        );
    }

    #[test]
    fn newest_result_wins_over_an_older_failure() {
        let results = [
            result("test", CheckOutcome::Passed),
            result("test", CheckOutcome::Failed),
        ];
        assert_eq!(evaluate(&names(&["test"]), &results), CiStatus::Passed);

        let rerunning = [
            result("test", CheckOutcome::Pending),
            result("test", CheckOutcome::Failed),
        ];
        assert_eq!(evaluate(&names(&["test"]), &rerunning), CiStatus::Pending);
    }

    #[test]
    fn an_unrequired_failure_is_ignored() {
        let results = [
            result("lint", CheckOutcome::Failed),
            result("test", CheckOutcome::Passed),
        ];
        assert_eq!(evaluate(&names(&["test"]), &results), CiStatus::Passed);
    }
}
