//! `FakeGitHub`: an in-memory [`GitHubApi`] for the forge's tests.
//!
//! Commits are synthetic, minted from a counter; a commit's history is its
//! parent chain. A merge conflicts when its head is declared to conflict
//! with trunk, with a head the branch already contains, or with a set of
//! heads the branch contains together. Every call is logged, and any call
//! can be made to fail once with either kind.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use dorfq_core::{PrNumber, Sha};

use crate::api::{
    ApiError, CheckResult, Commit, Compare, CompareStatus, GitHubApi, Merge, PrDetail, PrSummary,
    RefEntry, RefUpdate,
};
use crate::branch;

/// The kind of a failure injected with [`FakeGitHub::fail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Transient,
    Permanent,
}

#[derive(Debug, Clone)]
pub struct FakePr {
    pub open: bool,
    pub base: String,
    pub head: Sha,
    /// `None` for a head in a fork.
    pub head_branch: Option<String>,
    pub labels: Vec<String>,
    pub title: String,
    pub mergeable: Option<bool>,
    pub merged: bool,
}

#[derive(Debug, Default)]
pub struct State {
    pub refs: BTreeMap<String, Sha>,
    pub commits: BTreeMap<Sha, Commit>,
    pub prs: BTreeMap<u64, FakePr>,
    /// Check results per commit, newest first.
    pub checks: BTreeMap<Sha, Vec<CheckResult>>,
    pub comments: BTreeMap<u64, Vec<String>>,
    pub labels: BTreeSet<String>,
    /// Heads that conflict with trunk whatever they are merged onto.
    pub conflicts_with_trunk: BTreeSet<Sha>,
    /// Unordered pairs of heads that conflict with each other.
    pub conflicts: BTreeSet<(Sha, Sha)>,
    /// Heads that conflict only with every head of a set at once.
    pub conflicts_with_all: Vec<(Sha, Vec<Sha>)>,
    /// A ref moved to a sha just before the next update of that ref, as a
    /// push racing the forge would.
    pub interpose: Option<(String, Sha)>,
    /// A ref whose next update applies and then fails `Transient`, as a
    /// response lost in transit would.
    pub lose_update: Option<String>,
    /// A pull request whose next base change applies and then fails
    /// `Transient`, as a response lost in transit would.
    pub lose_set_base: Option<u64>,
    /// Pull requests every listing returns a second time, at the given head,
    /// as a page boundary shifting under a paginated read does.
    pub listed_again: Vec<(u64, Sha)>,
    /// Open pull requests every listing leaves out, as a page boundary
    /// shifting the other way does.
    pub unlisted: BTreeSet<u64>,
    /// Every call, as `"<method> <arguments>"`.
    pub calls: Vec<String>,
    /// Failures to inject: method, a substring of the call's arguments,
    /// kind; in order.
    failures: Vec<(&'static str, String, Failure)>,
    next: u64,
}

/// A handle on a shared fake repository; clones see the same state.
#[derive(Debug, Clone, Default)]
pub struct FakeGitHub(pub Rc<RefCell<State>>);

impl FakeGitHub {
    /// A repository with branch `main` at a root commit and the label
    /// `queue`.
    pub fn new() -> FakeGitHub {
        let fake = FakeGitHub::default();
        let root = fake.commit(&[]);
        fake.set_ref(&branch("main"), &root);
        fake.state().labels.insert("queue".to_owned());
        fake
    }

    pub fn state(&self) -> std::cell::RefMut<'_, State> {
        self.0.borrow_mut()
    }

    /// A new commit with `parents`, its own tree and a synthetic message.
    pub fn commit(&self, parents: &[Sha]) -> Sha {
        let mut state = self.state();
        let tree = state.mint();
        let sha = state.mint();
        state.commits.insert(
            sha.clone(),
            Commit {
                message: format!("commit {sha}"),
                tree,
                parents: parents.to_vec(),
            },
        );
        sha
    }

    pub fn trunk(&self) -> Sha {
        self.get(&branch("main")).expect("main exists")
    }

    pub fn get(&self, name: &str) -> Option<Sha> {
        self.state().refs.get(name).cloned()
    }

    pub fn set_ref(&self, name: &str, sha: &Sha) {
        self.state().refs.insert(name.to_owned(), sha.clone());
    }

    /// A commit on top of trunk, and trunk moved to it.
    pub fn advance_trunk(&self) -> Sha {
        let next = self.commit(&[self.trunk()]);
        self.set_ref(&branch("main"), &next);
        next
    }

    /// An open pull request against `main` whose head is a new commit on
    /// trunk, on branch `pr-<number>`, labelled `queue`, mergeable; its head.
    pub fn open_pr(&self, number: u64) -> Sha {
        let trunk = self.trunk();
        self.open_on(number, "main", &trunk)
    }

    /// An open pull request whose head is a new commit on `on`'s head and
    /// whose base is `on`'s head branch, on branch `pr-<number>`, labelled
    /// `queue`, mergeable; its head.
    pub fn stack_pr(&self, number: u64, on: u64) -> Sha {
        let (base, parent) = {
            let state = self.state();
            let on = &state.prs[&on];
            let base = on.head_branch.clone().expect("the base has a head branch");
            (base, on.head.clone())
        };
        self.open_on(number, &base, &parent)
    }

    fn open_on(&self, number: u64, base: &str, parent: &Sha) -> Sha {
        let head = self.commit(std::slice::from_ref(parent));
        let head_branch = format!("pr-{number}");
        self.set_ref(&branch(&head_branch), &head);
        self.state().prs.insert(
            number,
            FakePr {
                open: true,
                base: base.to_owned(),
                head: head.clone(),
                head_branch: Some(head_branch),
                labels: vec!["queue".to_owned()],
                title: format!("Change {number}"),
                mergeable: Some(true),
                merged: false,
            },
        );
        head
    }

    /// Moves pull request `number`'s head, and its head branch with it.
    pub fn push(&self, number: u64, sha: &Sha) {
        let mut state = self.state();
        let pr = state.prs.get_mut(&number).expect("pull request exists");
        pr.head = sha.clone();
        if let Some(name) = pr.head_branch.clone() {
            state.refs.insert(branch(&name), sha.clone());
        }
    }

    pub fn pr(&self, number: u64) -> std::cell::RefMut<'_, FakePr> {
        std::cell::RefMut::map(self.state(), |s| {
            s.prs.get_mut(&number).expect("pull request exists")
        })
    }

    pub fn set_checks(&self, sha: &Sha, results: Vec<CheckResult>) {
        self.state().checks.insert(sha.clone(), results);
    }

    pub fn comments(&self, number: u64) -> Vec<String> {
        self.state()
            .comments
            .get(&number)
            .cloned()
            .unwrap_or_default()
    }

    /// Makes the next call to `method` fail with `kind`.
    pub fn fail(&self, method: &'static str, kind: Failure) {
        self.fail_on(method, "", kind);
    }

    /// Makes the next call to `method` whose logged arguments contain
    /// `needle` fail with `kind`.
    pub fn fail_on(&self, method: &'static str, needle: &str, kind: Failure) {
        self.state()
            .failures
            .push((method, needle.to_owned(), kind));
    }

    /// The calls made so far whose method is one of `methods`.
    pub fn calls_to(&self, methods: &[&str]) -> Vec<String> {
        self.state()
            .calls
            .iter()
            .filter(|c| methods.iter().any(|m| c.split(' ').next() == Some(m)))
            .cloned()
            .collect()
    }

    /// Every call that changes the repository.
    pub fn writes(&self) -> Vec<String> {
        self.calls_to(&[
            "create_ref",
            "update_ref",
            "delete_ref",
            "create_commit",
            "merge",
            "remove_label",
            "create_comment",
            "set_base",
        ])
    }

    fn enter(&self, method: &'static str, arguments: String) -> Result<(), ApiError> {
        let mut state = self.state();
        let failure = state
            .failures
            .iter()
            .position(|(m, needle, _)| *m == method && arguments.contains(needle.as_str()))
            .map(|i| state.failures.remove(i).2);
        state.calls.push(format!("{method} {arguments}"));
        match failure {
            None => Ok(()),
            Some(Failure::Transient) => {
                Err(ApiError::Transient(format!("{method}: injected").into()))
            }
            Some(Failure::Permanent) => {
                Err(ApiError::Permanent(format!("{method}: injected").into()))
            }
        }
    }
}

impl State {
    fn mint(&mut self) -> Sha {
        self.next += 1;
        Sha::parse(&format!("{:040x}", self.next)).expect("forty hex digits")
    }

    /// `sha` and every commit in its history.
    fn history(&self, sha: &Sha) -> BTreeSet<Sha> {
        let mut seen = BTreeSet::new();
        let mut todo = vec![sha.clone()];
        while let Some(next) = todo.pop() {
            if seen.insert(next.clone())
                && let Some(commit) = self.commits.get(&next)
            {
                todo.extend(commit.parents.iter().cloned());
            }
        }
        seen
    }

    fn conflict(&self, a: &Sha, b: &Sha) -> bool {
        self.conflicts.contains(&(a.clone(), b.clone()))
            || self.conflicts.contains(&(b.clone(), a.clone()))
    }
}

fn permanent(message: String) -> ApiError {
    ApiError::Permanent(message.into())
}

impl GitHubApi for FakeGitHub {
    fn get_ref(&mut self, name: &str) -> Result<Option<Sha>, ApiError> {
        self.enter("get_ref", name.to_owned())?;
        Ok(self.get(name))
    }

    fn list_refs(&mut self, prefix: &str) -> Result<Vec<RefEntry>, ApiError> {
        self.enter("list_refs", prefix.to_owned())?;
        Ok(self
            .state()
            .refs
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(name, sha)| RefEntry {
                name: name.clone(),
                sha: sha.clone(),
            })
            .collect())
    }

    fn create_ref(&mut self, name: &str, sha: &Sha) -> Result<(), ApiError> {
        self.enter("create_ref", format!("{name} {sha}"))?;
        let mut state = self.state();
        if state.refs.contains_key(name) {
            return Err(permanent(format!("create_ref {name}: already exists")));
        }
        state.refs.insert(name.to_owned(), sha.clone());
        Ok(())
    }

    fn update_ref(&mut self, name: &str, sha: &Sha, force: bool) -> Result<RefUpdate, ApiError> {
        self.enter("update_ref", format!("{name} {sha} force={force}"))?;
        let mut state = self.state();
        if let Some((target, moved)) = state.interpose.take() {
            if target == name {
                state.refs.insert(target, moved);
            } else {
                state.interpose = Some((target, moved));
            }
        }
        let current = state
            .refs
            .get(name)
            .cloned()
            .ok_or_else(|| permanent(format!("update_ref {name}: no such ref")))?;
        if !force && !state.history(sha).contains(&current) {
            return Ok(RefUpdate::NotFastForward);
        }
        state.refs.insert(name.to_owned(), sha.clone());
        if state.lose_update.as_deref() == Some(name) {
            state.lose_update = None;
            return Err(ApiError::Transient(
                format!("update_ref {name}: response lost").into(),
            ));
        }
        Ok(RefUpdate::Updated)
    }

    fn delete_ref(&mut self, name: &str) -> Result<(), ApiError> {
        self.enter("delete_ref", name.to_owned())?;
        self.state().refs.remove(name);
        Ok(())
    }

    fn get_commit(&mut self, sha: &Sha) -> Result<Commit, ApiError> {
        self.enter("get_commit", sha.to_string())?;
        self.state()
            .commits
            .get(sha)
            .cloned()
            .ok_or_else(|| permanent(format!("get_commit {sha}: no such commit")))
    }

    fn create_commit(
        &mut self,
        message: &str,
        tree: &Sha,
        parents: &[Sha],
    ) -> Result<Sha, ApiError> {
        self.enter("create_commit", format!("{tree} {parents:?} {message:?}"))?;
        let mut state = self.state();
        let sha = state.mint();
        state.commits.insert(
            sha.clone(),
            Commit {
                message: message.to_owned(),
                tree: tree.clone(),
                parents: parents.to_vec(),
            },
        );
        Ok(sha)
    }

    fn merge(&mut self, base_branch: &str, head: &Sha, message: &str) -> Result<Merge, ApiError> {
        self.enter("merge", format!("{base_branch} {head} {message:?}"))?;
        let name = branch(base_branch);
        let mut state = self.state();
        let tip = state
            .refs
            .get(&name)
            .cloned()
            .ok_or_else(|| permanent(format!("merge into {base_branch}: no such branch")))?;
        let contained = state.history(&tip);
        if contained.contains(head) {
            return Ok(Merge::NothingToMerge);
        }
        let heads: Vec<&Sha> = state.prs.values().map(|pr| &pr.head).collect();
        let clashes = state.conflicts_with_trunk.contains(head)
            || heads
                .iter()
                .any(|h| contained.contains(*h) && state.conflict(h, head))
            || state
                .conflicts_with_all
                .iter()
                .any(|(h, set)| h == head && set.iter().all(|s| contained.contains(s)));
        if clashes {
            return Ok(Merge::Conflict);
        }
        let tree = state.mint();
        let sha = state.mint();
        state.commits.insert(
            sha.clone(),
            Commit {
                message: message.to_owned(),
                tree,
                parents: vec![tip, head.clone()],
            },
        );
        state.refs.insert(name, sha.clone());
        Ok(Merge::Merged(sha))
    }

    fn is_ancestor(&mut self, ancestor: &Sha, descendant: &Sha) -> Result<bool, ApiError> {
        self.enter("is_ancestor", format!("{ancestor} {descendant}"))?;
        Ok(self.state().history(descendant).contains(ancestor))
    }

    fn compare(&mut self, base: &Sha, head: &Sha) -> Result<Compare, ApiError> {
        self.enter("compare", format!("{base} {head}"))?;
        let state = self.state();
        let (of_base, of_head) = (state.history(base), state.history(head));
        let status = match (of_head.contains(base), of_base.contains(head)) {
            (true, true) => CompareStatus::Identical,
            (true, false) => CompareStatus::Ahead,
            (false, true) => CompareStatus::Behind,
            (false, false) => CompareStatus::Diverged,
        };
        let ahead_by = of_head.difference(&of_base).count();
        // Shas are minted in order and a commit after its parents, so the
        // greatest common ancestor has no other common ancestor descending
        // from it.
        let merge_base = of_base
            .intersection(&of_head)
            .max()
            .expect("fake histories share a root")
            .clone();
        Ok(Compare {
            status,
            ahead_by: u32::try_from(ahead_by).expect("a fake history fits u32"),
            merge_base,
        })
    }

    fn list_open_prs(&mut self) -> Result<Vec<PrSummary>, ApiError> {
        self.enter("list_open_prs", String::new())?;
        let state = self.state();
        let summary = |number: u64, pr: &FakePr, head: &Sha| PrSummary {
            number: PrNumber(number),
            head: head.clone(),
            base: pr.base.clone(),
            head_branch: pr.head_branch.clone(),
            labels: pr.labels.clone(),
            title: pr.title.clone(),
        };
        let mut listing: Vec<PrSummary> = state
            .prs
            .iter()
            .filter(|(number, pr)| pr.open && !state.unlisted.contains(number))
            .map(|(&number, pr)| summary(number, pr, &pr.head))
            .collect();
        for (number, head) in &state.listed_again {
            listing.push(summary(*number, &state.prs[number], head));
        }
        Ok(listing)
    }

    fn get_pr(&mut self, number: PrNumber) -> Result<PrDetail, ApiError> {
        self.enter("get_pr", number.0.to_string())?;
        let state = self.state();
        let pr = state
            .prs
            .get(&number.0)
            .ok_or_else(|| permanent(format!("get_pr {}: no such pull request", number.0)))?;
        Ok(PrDetail {
            open: pr.open,
            base: pr.base.clone(),
            head: pr.head.clone(),
            labels: pr.labels.clone(),
            mergeable: pr.mergeable,
            merged: pr.merged,
        })
    }

    fn set_base(&mut self, number: PrNumber, base: &str) -> Result<(), ApiError> {
        self.enter("set_base", format!("{} {base}", number.0))?;
        let mut state = self.state();
        let pr = state
            .prs
            .get_mut(&number.0)
            .ok_or_else(|| permanent(format!("set_base {}: no such pull request", number.0)))?;
        pr.base = base.to_owned();
        pr.mergeable = None;
        if state.lose_set_base == Some(number.0) {
            state.lose_set_base = None;
            return Err(ApiError::Transient(
                format!("set_base {}: response lost", number.0).into(),
            ));
        }
        Ok(())
    }

    fn merged_pr_heads(&mut self, head_branch: &str, base: &str) -> Result<Vec<Sha>, ApiError> {
        self.enter("merged_pr_heads", format!("{head_branch} {base}"))?;
        Ok(self
            .state()
            .prs
            .values()
            .filter(|pr| {
                !pr.open
                    && pr.merged
                    && pr.head_branch.as_deref() == Some(head_branch)
                    && pr.base == base
            })
            .map(|pr| pr.head.clone())
            .collect())
    }

    fn checks(&mut self, sha: &Sha) -> Result<Vec<CheckResult>, ApiError> {
        self.enter("checks", sha.to_string())?;
        Ok(self.state().checks.get(sha).cloned().unwrap_or_default())
    }

    fn remove_label(&mut self, number: PrNumber, label: &str) -> Result<(), ApiError> {
        self.enter("remove_label", format!("{} {label}", number.0))?;
        if let Some(pr) = self.state().prs.get_mut(&number.0) {
            pr.labels.retain(|l| l != label);
        }
        Ok(())
    }

    fn list_comments(&mut self, number: PrNumber) -> Result<Vec<String>, ApiError> {
        self.enter("list_comments", number.0.to_string())?;
        Ok(self.comments(number.0))
    }

    fn create_comment(&mut self, number: PrNumber, body: &str) -> Result<(), ApiError> {
        self.enter("create_comment", number.0.to_string())?;
        self.state()
            .comments
            .entry(number.0)
            .or_default()
            .push(body.to_owned());
        Ok(())
    }

    fn label_exists(&mut self, label: &str) -> Result<bool, ApiError> {
        self.enter("label_exists", label.to_owned())?;
        Ok(self.state().labels.contains(label))
    }
}

mod tests {
    use super::FakeGitHub;
    use crate::api::GitHubApi;

    #[test]
    fn merge_base_is_the_fork_point_or_the_ancestor_itself() {
        let mut fake = FakeGitHub::new();
        let fork = fake.trunk();
        let head = fake.open_pr(1);
        let ahead = fake.commit(std::slice::from_ref(&head));
        let trunk = fake.advance_trunk();
        fake.advance_trunk();
        let trunk_now = fake.trunk();
        assert_eq!(fake.compare(&ahead, &trunk_now).unwrap().merge_base, fork);
        assert_eq!(fake.compare(&trunk_now, &ahead).unwrap().merge_base, fork);
        assert_eq!(fake.compare(&trunk, &trunk_now).unwrap().merge_base, trunk);
        assert_eq!(fake.compare(&head, &ahead).unwrap().merge_base, head);
    }
}
