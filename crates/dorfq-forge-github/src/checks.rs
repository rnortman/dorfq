//! Required-check evaluation: the results on one commit, reduced to a
//! [`CiStatus`] by the check names the configuration requires.

use std::collections::BTreeSet;

use dorfq_core::{CheckName, CiStatus, Failure};

use crate::api::{CheckOutcome, CheckResult};

/// The status of `required` among `results`, which are newest first.
///
/// Each required name is decided by its newest result, so a re-run
/// supersedes the run it repeats; a name with no result is pending. Any
/// required name failed makes the whole `Failed`, naming every required
/// check that failed, since the commit cannot pass whatever is still
/// running; otherwise any pending makes it `Pending`. Results under names
/// that are not required are ignored.
pub fn evaluate(required: &[String], results: &[CheckResult]) -> CiStatus {
    let mut pending = false;
    let mut failed = BTreeSet::new();
    for name in required {
        match results.iter().find(|r| &r.name == name).map(|r| r.outcome) {
            Some(CheckOutcome::Failed) => {
                failed.insert(CheckName(name.clone()));
            }
            Some(CheckOutcome::Passed) => {}
            Some(CheckOutcome::Pending) | None => pending = true,
        }
    }
    if !failed.is_empty() {
        CiStatus::Failed(Failure::new(failed))
    } else if pending {
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

    fn failed(names: &[&str]) -> CiStatus {
        CiStatus::Failed(Failure::new(
            names.iter().map(|n| CheckName((*n).to_owned())).collect(),
        ))
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
            failed(&["test"])
        );
    }

    #[test]
    fn failed_names_every_failed_required_check_and_no_other() {
        let results = [
            result("build", CheckOutcome::Failed),
            result("lint", CheckOutcome::Failed),
            result("test", CheckOutcome::Failed),
            result("docs", CheckOutcome::Passed),
        ];
        assert_eq!(
            evaluate(&names(&["test", "build", "docs"]), &results),
            failed(&["build", "test"])
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
