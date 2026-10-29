//! The GitHub REST surface the forge uses: the [`GitHubApi`] trait, its
//! typed results, and [`RestClient`], which implements it over HTTPS.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dorfq_core::{ForgeError, PrNumber, Sha};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppKey;

/// Every call the forge makes on a repository.
pub trait GitHubApi {
    /// The sha a full ref name (`refs/heads/main`) points at; `None` when
    /// the ref does not exist.
    fn get_ref(&mut self, name: &str) -> Result<Option<Sha>, ApiError>;
    /// Every ref whose full name starts with `prefix`.
    fn list_refs(&mut self, prefix: &str) -> Result<Vec<RefEntry>, ApiError>;
    fn create_ref(&mut self, name: &str, sha: &Sha) -> Result<(), ApiError>;
    /// Moves a ref; without `force`, only as a fast-forward.
    fn update_ref(&mut self, name: &str, sha: &Sha, force: bool) -> Result<RefUpdate, ApiError>;
    /// Deletes a ref; an absent ref is not an error.
    fn delete_ref(&mut self, name: &str) -> Result<(), ApiError>;
    fn get_commit(&mut self, sha: &Sha) -> Result<Commit, ApiError>;
    fn create_commit(
        &mut self,
        message: &str,
        tree: &Sha,
        parents: &[Sha],
    ) -> Result<Sha, ApiError>;
    /// Merges `head` into the branch `base_branch` (a short branch name).
    fn merge(&mut self, base_branch: &str, head: &Sha, message: &str) -> Result<Merge, ApiError>;
    /// True when `descendant` is `ancestor` or has it in its history.
    fn is_ancestor(&mut self, ancestor: &Sha, descendant: &Sha) -> Result<bool, ApiError>;
    /// How `head` stands to `base`, the commits `head` has that `base` does
    /// not, and their merge base.
    fn compare(&mut self, base: &Sha, head: &Sha) -> Result<Compare, ApiError>;
    /// The numbers of the open pull requests carrying `label`, in the
    /// listing's order; a number may appear twice.
    fn list_labelled(&mut self, label: &str) -> Result<Vec<PrNumber>, ApiError>;
    /// The lowest-numbered open pull request whose head branch is `branch`
    /// in this repository.
    fn open_pr_with_head(&mut self, branch: &str) -> Result<Option<OpenPr>, ApiError>;
    fn get_pr(&mut self, number: PrNumber) -> Result<PrDetail, ApiError>;
    /// Changes the branch a pull request targets.
    fn set_base(&mut self, number: PrNumber, base: &str) -> Result<(), ApiError>;
    /// The heads, as merged, of the closed and merged pull requests whose head
    /// branch is `branch` in this repository and whose base is `base`.
    fn merged_pr_heads(&mut self, branch: &str, base: &str) -> Result<Vec<Sha>, ApiError>;
    /// Check runs and commit statuses on `sha`, newest first.
    fn checks(&mut self, sha: &Sha) -> Result<Vec<CheckResult>, ApiError>;
    /// Removes a label from a pull request; an absent label is not an error.
    fn remove_label(&mut self, number: PrNumber, label: &str) -> Result<(), ApiError>;
    /// Comment bodies on a pull request, oldest first.
    fn list_comments(&mut self, number: PrNumber) -> Result<Vec<String>, ApiError>;
    fn create_comment(&mut self, number: PrNumber, body: &str) -> Result<(), ApiError>;
    /// What issue `number` is; `None` when there is no such issue.
    fn get_issue(&mut self, number: u64) -> Result<Option<IssueDetail>, ApiError>;
    /// Replaces issue `number`'s body.
    fn set_issue_body(&mut self, number: u64, body: &str) -> Result<(), ApiError>;
    fn label_exists(&mut self, label: &str) -> Result<bool, ApiError>;
    /// The repository ruleset named `name`: its id and the branches, by
    /// short name, it includes. Two of that name is an error.
    fn find_ruleset(
        &mut self,
        name: &str,
    ) -> Result<Option<(RulesetId, BTreeSet<String>)>, ApiError>;
    /// Creates a ruleset named `name` that refuses every update of
    /// `branches` (short names) to everyone.
    fn create_ruleset(
        &mut self,
        name: &str,
        branches: &BTreeSet<String>,
    ) -> Result<RulesetId, ApiError>;
    /// Replaces the branches ruleset `id` includes; `false` when the
    /// ruleset no longer exists.
    fn update_ruleset(
        &mut self,
        id: RulesetId,
        branches: &BTreeSet<String>,
    ) -> Result<bool, ApiError>;
    /// Deletes ruleset `id`; an absent ruleset is not an error.
    fn delete_ruleset(&mut self, id: RulesetId) -> Result<(), ApiError>;
    /// Every Actions workflow run whose head commit is `head`.
    fn list_runs(&mut self, head: &Sha) -> Result<Vec<WorkflowRun>, ApiError>;
    /// Cancels a workflow run; one already finished, or gone, is not an
    /// error.
    fn cancel_run(&mut self, id: RunId) -> Result<(), ApiError>;
    /// The instant before which GitHub asked for no further requests, as the
    /// latest rate-limited response since the last call said; cleared by
    /// the call. Not a request.
    fn rate_limit_pause(&mut self) -> Option<SystemTime>;
}

/// A repository ruleset's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RulesetId(pub u64);

/// An Actions workflow run's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunId(pub u64);

/// One Actions workflow run on a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    pub id: RunId,
    /// The branch the run was triggered on, as GitHub recorded it; GitHub
    /// records none for some runs.
    pub head_branch: Option<String>,
    pub completed: bool,
}

/// One ref and the sha it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// The full name, `refs/...`.
    pub name: String,
    pub sha: Sha,
}

/// The outcome of a ref update GitHub accepted for processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefUpdate {
    Updated,
    /// Refused: the update was not a fast-forward and `force` was off.
    NotFastForward,
}

/// A git commit object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub message: String,
    pub tree: Sha,
    pub parents: Vec<Sha>,
}

/// The outcome of a server-side merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    /// The merge commit now at the tip of the branch.
    Merged(Sha),
    /// The head is already contained in the branch; nothing changed.
    NothingToMerge,
    /// Git refused the merge.
    Conflict,
}

/// Two commits compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compare {
    pub status: CompareStatus,
    /// Commits in the head's history that are not in the base's.
    pub ahead_by: u32,
    /// The best common ancestor of the two.
    pub merge_base: Sha,
}

/// Where the head of a comparison stands to its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareStatus {
    /// The same commit.
    Identical,
    /// The head has the base in its history.
    Ahead,
    /// The base has the head in its history.
    Behind,
    /// Neither has the other.
    Diverged,
}

/// An open pull request, as the list endpoint reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPr {
    pub number: PrNumber,
    pub head: Sha,
    /// The branch it targets.
    pub base: String,
}

/// One pull request, as the single-item endpoint reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrDetail {
    pub open: bool,
    /// The branch it targets.
    pub base: String,
    pub head: Sha,
    /// Its head branch's name, when the head lives in this repository;
    /// `None` for a head in a fork, which no pull request here can target.
    pub head_branch: Option<String>,
    pub title: String,
    pub labels: Vec<String>,
    /// GitHub's verdict on whether it may merge into its base branch.
    pub merge_state: MergeState,
    /// Whether it is a draft.
    pub draft: bool,
    /// Whether it was merged, by any hand.
    pub merged: bool,
}

/// One issue, as the single-item endpoint reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssueDetail {
    /// The issue is a pull request: GitHub lists pull requests as issues
    /// carrying a `pull_request` key.
    pub pull_request: bool,
}

/// A pull request's `mergeable_state`: whether GitHub would merge it into
/// its base branch now, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeState {
    /// Mergeable, every requirement met.
    Clean,
    /// Mergeable, with a check that is not required failing.
    Unstable,
    /// Mergeable, with pre-receive hooks.
    HasHooks,
    /// Mergeable, but its head is behind the base branch.
    Behind,
    /// The base branch's protection rules are not satisfied.
    Blocked,
    /// It conflicts with its base branch.
    Dirty,
    /// It is a draft.
    Draft,
    /// GitHub has not computed it yet.
    Unknown,
}

impl MergeState {
    fn parse(word: &str) -> Option<MergeState> {
        Some(match word {
            "clean" => MergeState::Clean,
            "unstable" => MergeState::Unstable,
            "has_hooks" => MergeState::HasHooks,
            "behind" => MergeState::Behind,
            "blocked" => MergeState::Blocked,
            "dirty" => MergeState::Dirty,
            "draft" => MergeState::Draft,
            "unknown" => MergeState::Unknown,
            _ => return None,
        })
    }
}

/// One check run or commit status, reduced to what the forge decides on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub name: String,
    pub outcome: CheckOutcome,
}

/// Where one check run or commit status stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    Pending,
    Passed,
    Failed,
}

/// A failed API call, by whether retrying later can help.
#[derive(Debug)]
pub enum ApiError {
    Transient(Box<dyn Error + Send + Sync>),
    Permanent(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Transient(e) => write!(f, "transient GitHub error: {e}"),
            ApiError::Permanent(e) => write!(f, "permanent GitHub error: {e}"),
        }
    }
}

impl Error for ApiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ApiError::Transient(e) | ApiError::Permanent(e) => Some(e.as_ref()),
        }
    }
}

impl From<ApiError> for ForgeError {
    fn from(error: ApiError) -> ForgeError {
        match error {
            ApiError::Transient(e) => ForgeError::Transient(e),
            ApiError::Permanent(e) => ForgeError::Permanent(e),
        }
    }
}

fn permanent(message: String) -> ApiError {
    ApiError::Permanent(message.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
    Patch,
    Put,
    Delete,
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        })
    }
}

struct HttpRequest<'a> {
    method: Method,
    url: &'a str,
    /// The `Authorization` header's value.
    authorization: &'a str,
    body: Option<&'a Value>,
}

struct HttpResponse {
    status: u16,
    /// Header names in lowercase.
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn json(&self, what: &str) -> Result<Value, ApiError> {
        if self.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&self.body)
            .map_err(|e| permanent(format!("{what}: response is not JSON: {e}")))
    }
}

/// A request that produced no response.
struct TransportError {
    transient: bool,
    message: String,
}

type Transport = Box<dyn FnMut(&HttpRequest<'_>) -> Result<HttpResponse, TransportError>>;

type Clock = Box<dyn Fn() -> SystemTime>;

/// How requests are authorised.
#[derive(Clone)]
pub enum Auth {
    /// A fixed token: `Bearer <token>` on every request.
    Token(String),
    /// A GitHub App: installation tokens minted from its key.
    App(AppKey),
}

enum Credential {
    /// The full header value.
    Token(String),
    App {
        key: AppKey,
        /// The repository's name, which the minted token is scoped to.
        repo: String,
        /// Found once and kept: an installation's id does not change while
        /// the app stays installed.
        installation: Option<u64>,
        token: Option<InstallationToken>,
    },
}

struct InstallationToken {
    /// The full header value.
    bearer: String,
    /// The local clock, read before the mint request was sent.
    minted: SystemTime,
}

/// GitHub's documented lifetime of an installation token. A token's age is
/// measured on the local clock from a time read before the mint request, so
/// the measure starts no later than GitHub's hour does and the offset
/// between the two clocks does not enter. The response's `expires_at` is a
/// time on GitHub's clock and is not read.
const INSTALLATION_TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// A token with no more than this left of its lifetime is replaced before
/// the next request. It is ten times a request's thirty-second timeout, so a
/// token that passed the check cannot expire before its request is answered.
const INSTALLATION_TOKEN_MARGIN: Duration = Duration::from_secs(5 * 60);

/// What GitHub's 401 to a JWT-authorised request means.
const JWT_REJECTED: &str = "GitHub rejected the app's JWT: wrong client id, a key that is not \
     this app's, or this host's clock more than a minute ahead or five minutes behind GitHub's";

/// What GitHub's 401 to a request under a fresh installation token means.
const INSTALLATION_TOKEN_REJECTED: &str = "the installation token was rejected: the app was \
     suspended or uninstalled, its key revoked, or this host's clock stepped back";

/// [`GitHubApi`] over GitHub's REST API (github.com or GitHub Enterprise).
pub struct RestClient {
    /// `api_base` without a trailing slash.
    api_base: String,
    /// `/repos/{owner}/{repo}`, encoded.
    repo_path: String,
    /// `owner/repo`, unencoded.
    repo_name: String,
    /// The repository's owner, unencoded.
    owner: String,
    credential: Credential,
    transport: Transport,
    now: Clock,
    /// The end of the latest rate-limit pause a response asked for, until
    /// [`GitHubApi::rate_limit_pause`] takes it.
    paused_until: Option<SystemTime>,
}

const PER_PAGE: &str = "per_page=100";

impl RestClient {
    /// A client for one repository. `api_base` is `https://api.github.com`
    /// or a GitHub Enterprise `https://<host>/api/v3`; `auth` says what
    /// every request is authorised with.
    pub fn new(
        api_base: &str,
        owner: &str,
        repo: &str,
        auth: Auth,
        user_agent: &str,
    ) -> RestClient {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .https_only(true)
            // A redirect is answered as received: following it would drop
            // the bearer token, and a 3xx is mapped by `failure`.
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(user_agent)
            .build()
            .into();
        let transport: Transport = Box::new(move |request: &HttpRequest<'_>| send(&agent, request));
        RestClient::with_transport(
            api_base,
            owner,
            repo,
            auth,
            transport,
            Box::new(SystemTime::now),
        )
    }

    fn with_transport(
        api_base: &str,
        owner: &str,
        repo: &str,
        auth: Auth,
        transport: Transport,
        now: Clock,
    ) -> RestClient {
        let credential = match auth {
            Auth::Token(token) => Credential::Token(format!("Bearer {token}")),
            Auth::App(key) => Credential::App {
                key,
                repo: repo.to_owned(),
                installation: None,
                token: None,
            },
        };
        RestClient {
            api_base: api_base.trim_end_matches('/').to_owned(),
            repo_path: format!("/repos/{}/{}", encode(owner), encode(repo)),
            repo_name: format!("{owner}/{repo}"),
            owner: owner.to_owned(),
            credential,
            transport,
            now,
            paused_until: None,
        }
    }

    fn repo_url(&self) -> String {
        format!("{}{}", self.api_base, self.repo_path)
    }

    /// The `Authorization` value for the next request; under an app, mints
    /// an installation token first when none is cached or the cached one is
    /// within the margin of its lifetime.
    fn authorization(&mut self) -> Result<String, ApiError> {
        let RestClient {
            api_base,
            repo_path,
            repo_name: _,
            owner: _,
            credential,
            transport,
            now: clock,
            paused_until,
        } = self;
        let (key, repo, installation, token) = match credential {
            Credential::Token(bearer) => return Ok(bearer.clone()),
            Credential::App {
                key,
                repo,
                installation,
                token,
            } => (key, repo, installation, token),
        };
        let now = clock();
        if let Some(cached) = token {
            // A clock that stepped back since the mint makes the token look
            // younger, never older.
            let age = now.duration_since(cached.minted).unwrap_or_default();
            if age < INSTALLATION_TOKEN_LIFETIME - INSTALLATION_TOKEN_MARGIN {
                return Ok(cached.bearer.clone());
            }
        }
        let jwt = key
            .jwt(now)
            .map_err(|e| permanent(format!("signing the app's JWT: {e}")))?;
        let jwt = format!("Bearer {jwt}");
        let id = match installation {
            Some(id) => *id,
            None => {
                let doing = "finding the app's installation";
                let path = format!("{repo_path}/installation");
                let url = format!("{api_base}{path}");
                let found: InstallationJson = app_call(
                    (transport, clock, paused_until),
                    doing,
                    Method::Get,
                    &path,
                    &url,
                    &jwt,
                    None,
                )?;
                *installation = Some(found.id);
                found.id
            }
        };
        let doing = "minting an installation token";
        let path = format!("/app/installations/{id}/access_tokens");
        let url = format!("{api_base}{path}");
        let body = json!({ "repositories": [repo.as_str()] });
        let minted: AccessTokenJson = app_call(
            (transport, clock, paused_until),
            doing,
            Method::Post,
            &path,
            &url,
            &jwt,
            Some(&body),
        )?;
        let bearer = format!("Bearer {}", minted.token);
        *token = Some(InstallationToken {
            bearer: bearer.clone(),
            minted: now,
        });
        Ok(bearer)
    }

    /// `GET {repo}{path}`; the decoded body.
    pub fn get(&mut self, path: &str) -> Result<Value, ApiError> {
        self.json_call(Method::Get, path, None)
    }

    /// `POST {repo}{path}` with a JSON body; the decoded response body.
    pub fn post(&mut self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.json_call(Method::Post, path, Some(body))
    }

    /// `PATCH {repo}{path}` with a JSON body; the decoded response body.
    pub fn patch(&mut self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.json_call(Method::Patch, path, Some(body))
    }

    /// `DELETE {repo}{path}`.
    pub fn delete(&mut self, path: &str) -> Result<(), ApiError> {
        self.json_call(Method::Delete, path, None).map(|_| ())
    }

    fn json_call(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let response = self.call(method, path, body)?;
        if !(200..300).contains(&response.status) {
            return Err(failure(method, path, &response));
        }
        response.json(&format!("{method} {path}"))
    }

    /// One request against a repository path; any status is returned.
    fn call(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<HttpResponse, ApiError> {
        let url = format!("{}{path}", self.repo_url());
        self.call_url(method, path, &url, body)
    }

    fn call_url(
        &mut self,
        method: Method,
        path: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<HttpResponse, ApiError> {
        let authorization = self.authorization()?;
        let response = exchange(
            (&mut self.transport, &self.now, &mut self.paused_until),
            method,
            path,
            url,
            &authorization,
            body,
        )?;
        if response.status == 401 && matches!(self.credential, Credential::App { .. }) {
            return Err(permanent(format!(
                "{method} {path}: HTTP 401: {} ({INSTALLATION_TOKEN_REJECTED})",
                github_message(&response)
            )));
        }
        Ok(response)
    }

    /// Every page of a list endpoint, following `Link: rel="next"`.
    fn get_pages(
        &mut self,
        path: &str,
        classify: fn(Method, &str, &HttpResponse) -> ApiError,
    ) -> Result<Vec<Value>, ApiError> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut url = format!("{}{path}{separator}{PER_PAGE}", self.repo_url());
        let mut pages = Vec::new();
        loop {
            let response = self.call_url(Method::Get, path, &url, None)?;
            if response.status != 200 {
                return Err(classify(Method::Get, path, &response));
            }
            pages.push(response.json(&format!("GET {path}"))?);
            match response.header("link").and_then(next_link) {
                Some(next) => url = next,
                None => return Ok(pages),
            }
        }
    }

    fn get_array<T: for<'de> Deserialize<'de>>(&mut self, path: &str) -> Result<Vec<T>, ApiError> {
        let mut items = Vec::new();
        for page in self.get_pages(path, failure)? {
            items.extend(decode::<Vec<T>>(&format!("GET {path}"), page)?);
        }
        Ok(items)
    }
}

impl GitHubApi for RestClient {
    fn get_ref(&mut self, name: &str) -> Result<Option<Sha>, ApiError> {
        let path = format!("/git/ref/{}", ref_path(name)?);
        let response = self.call(Method::Get, &path, None)?;
        match response.status {
            200 => {
                let what = format!("GET {path}");
                let found: RefJson = decode(&what, response.json(&what)?)?;
                Ok(Some(parse_sha(&what, &found.object.sha)?))
            }
            404 => Ok(None),
            _ => Err(failure(Method::Get, &path, &response)),
        }
    }

    fn list_refs(&mut self, prefix: &str) -> Result<Vec<RefEntry>, ApiError> {
        let path = format!("/git/matching-refs/{}", ref_path(prefix)?);
        let what = format!("GET {path}");
        let found: Vec<RefJson> = self.get_array(&path)?;
        found
            .into_iter()
            .map(|r| {
                Ok(RefEntry {
                    sha: parse_sha(&what, &r.object.sha)?,
                    name: r.name,
                })
            })
            .collect()
    }

    fn create_ref(&mut self, name: &str, sha: &Sha) -> Result<(), ApiError> {
        ref_path(name)?;
        let body = json!({ "ref": name, "sha": sha.as_str() });
        self.json_call(Method::Post, "/git/refs", Some(&body))
            .map(|_| ())
    }

    fn update_ref(&mut self, name: &str, sha: &Sha, force: bool) -> Result<RefUpdate, ApiError> {
        let path = format!("/git/refs/{}", ref_path(name)?);
        let body = json!({ "sha": sha.as_str(), "force": force });
        let response = self.call(Method::Patch, &path, Some(&body))?;
        match response.status {
            200 => Ok(RefUpdate::Updated),
            422 if github_message(&response).contains("not a fast forward") => {
                Ok(RefUpdate::NotFastForward)
            }
            _ => Err(failure(Method::Patch, &path, &response)),
        }
    }

    fn delete_ref(&mut self, name: &str) -> Result<(), ApiError> {
        let path = format!("/git/refs/{}", ref_path(name)?);
        let response = self.call(Method::Delete, &path, None)?;
        match response.status {
            204 | 404 => Ok(()),
            // GitHub answers a delete of an absent ref with this 422.
            422 if github_message(&response).contains("Reference does not exist") => Ok(()),
            _ => Err(failure(Method::Delete, &path, &response)),
        }
    }

    fn get_commit(&mut self, sha: &Sha) -> Result<Commit, ApiError> {
        let path = format!("/git/commits/{sha}");
        let what = format!("GET {path}");
        let commit: CommitJson = decode(&what, self.json_call(Method::Get, &path, None)?)?;
        Ok(Commit {
            message: commit.message,
            tree: parse_sha(&what, &commit.tree.sha)?,
            parents: commit
                .parents
                .iter()
                .map(|p| parse_sha(&what, &p.sha))
                .collect::<Result<_, _>>()?,
        })
    }

    fn create_commit(
        &mut self,
        message: &str,
        tree: &Sha,
        parents: &[Sha],
    ) -> Result<Sha, ApiError> {
        let parents: Vec<&str> = parents.iter().map(Sha::as_str).collect();
        let body = json!({ "message": message, "tree": tree.as_str(), "parents": parents });
        let what = "POST /git/commits";
        let created: ShaJson = decode(
            what,
            self.json_call(Method::Post, "/git/commits", Some(&body))?,
        )?;
        parse_sha(what, &created.sha)
    }

    fn merge(&mut self, base_branch: &str, head: &Sha, message: &str) -> Result<Merge, ApiError> {
        let body = json!({ "base": base_branch, "head": head.as_str(), "commit_message": message });
        let what = "POST /merges";
        let response = self.call(Method::Post, "/merges", Some(&body))?;
        match response.status {
            201 => {
                let merged: ShaJson = decode(what, response.json(what)?)?;
                Ok(Merge::Merged(parse_sha(what, &merged.sha)?))
            }
            204 => Ok(Merge::NothingToMerge),
            409 => Ok(Merge::Conflict),
            _ => Err(failure(Method::Post, "/merges", &response)),
        }
    }

    fn is_ancestor(&mut self, ancestor: &Sha, descendant: &Sha) -> Result<bool, ApiError> {
        let compared = self.compare(ancestor, descendant)?;
        Ok(matches!(
            compared.status,
            CompareStatus::Identical | CompareStatus::Ahead
        ))
    }

    fn compare(&mut self, base: &Sha, head: &Sha) -> Result<Compare, ApiError> {
        let path = format!("/compare/{base}...{head}");
        let what = format!("GET {path}");
        let compared: CompareJson = decode(&what, self.json_call(Method::Get, &path, None)?)?;
        compare_of(&what, compared)
    }

    fn list_labelled(&mut self, label: &str) -> Result<Vec<PrNumber>, ApiError> {
        // The issues endpoint filters by label on GitHub's side and lists
        // pull requests as issues that carry a `pull_request` key.
        let path = format!("/issues?state=open&labels={}", encode(label));
        let found: Vec<IssueJson> = self.get_array(&path)?;
        Ok(found
            .into_iter()
            .filter(|issue| issue.pull_request.is_some())
            .map(|issue| PrNumber(issue.number))
            .collect())
    }

    fn open_pr_with_head(&mut self, branch: &str) -> Result<Option<OpenPr>, ApiError> {
        let path = format!(
            "/pulls?state=open&head={}",
            encode(&format!("{}:{branch}", self.owner))
        );
        let what = format!("GET {path}");
        let found: Vec<PrJson> = self.get_array(&path)?;
        let Some(lowest) = found.into_iter().min_by_key(|pr| pr.number) else {
            return Ok(None);
        };
        Ok(Some(OpenPr {
            number: PrNumber(lowest.number),
            head: parse_sha(&what, &lowest.head.sha)?,
            base: lowest.base.branch,
        }))
    }

    fn get_pr(&mut self, number: PrNumber) -> Result<PrDetail, ApiError> {
        let path = format!("/pulls/{}", number.0);
        let what = format!("GET {path}");
        let pr: PrJson = decode(&what, self.json_call(Method::Get, &path, None)?)?;
        let open = match pr.state.as_str() {
            "open" => true,
            "closed" => false,
            other => return Err(permanent(format!("{what}: unknown state {other:?}"))),
        };
        let merged = pr
            .merged
            .ok_or_else(|| permanent(format!("{what}: no merged field")))?;
        let word = pr
            .mergeable_state
            .ok_or_else(|| permanent(format!("{what}: no mergeable_state field")))?;
        let merge_state = MergeState::parse(&word)
            .ok_or_else(|| permanent(format!("{what}: unknown mergeable_state {word:?}")))?;
        let draft = pr
            .draft
            .ok_or_else(|| permanent(format!("{what}: no draft field")))?;
        // GitHub compares repository names case-insensitively.
        let local = pr
            .head
            .repo
            .as_ref()
            .is_some_and(|r| r.full_name.eq_ignore_ascii_case(&self.repo_name));
        Ok(PrDetail {
            open,
            base: pr.base.branch,
            head: parse_sha(&what, &pr.head.sha)?,
            head_branch: local.then_some(pr.head.branch),
            title: pr.title,
            labels: pr.labels.into_iter().map(|l| l.name).collect(),
            merge_state,
            draft,
            merged,
        })
    }

    fn set_base(&mut self, number: PrNumber, base: &str) -> Result<(), ApiError> {
        let path = format!("/pulls/{}", number.0);
        let response = self.call(Method::Patch, &path, Some(&json!({ "base": base })))?;
        match response.status {
            200 => Ok(()),
            _ => Err(failure(Method::Patch, &path, &response)),
        }
    }

    fn merged_pr_heads(&mut self, branch: &str, base: &str) -> Result<Vec<Sha>, ApiError> {
        let path = format!(
            "/pulls?state=closed&head={}&base={}",
            encode(&format!("{}:{branch}", self.owner)),
            encode(base)
        );
        let what = format!("GET {path}");
        let found: Vec<PrJson> = self.get_array(&path)?;
        found
            .into_iter()
            .filter(|pr| pr.merged_at.is_some())
            .map(|pr| parse_sha(&what, &pr.head.sha))
            .collect()
    }

    fn checks(&mut self, sha: &Sha) -> Result<Vec<CheckResult>, ApiError> {
        let mut timed: Vec<(Option<String>, CheckResult)> = Vec::new();

        let path = format!("/commits/{sha}/check-runs");
        let what = format!("GET {path}");
        for page in self.get_pages(&path, failure)? {
            let runs: CheckRunsJson = decode(&what, page)?;
            for run in runs.check_runs {
                let outcome = check_run_outcome(&what, &run)?;
                timed.push((
                    run.completed_at.or(run.started_at),
                    CheckResult {
                        name: run.name,
                        outcome,
                    },
                ));
            }
        }

        let path = format!("/commits/{sha}/status");
        let what = format!("GET {path}");
        for page in self.get_pages(&path, failure)? {
            let combined: CombinedStatusJson = decode(&what, page)?;
            for status in combined.statuses {
                let outcome = match status.state.as_str() {
                    "pending" => CheckOutcome::Pending,
                    "success" => CheckOutcome::Passed,
                    "failure" | "error" => CheckOutcome::Failed,
                    other => {
                        return Err(permanent(format!("{what}: unknown status state {other:?}")));
                    }
                };
                timed.push((
                    Some(status.updated_at),
                    CheckResult {
                        name: status.context,
                        outcome,
                    },
                ));
            }
        }

        // Newest first. GitHub's timestamps are UTC ISO 8601 of one width,
        // so text order is time order; a run with no timestamp is queued and
        // has not started, so it counts as the newest.
        timed.sort_by(|(a, _), (b, _)| match (a, b) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => b.cmp(a),
        });
        Ok(timed.into_iter().map(|(_, result)| result).collect())
    }

    fn remove_label(&mut self, number: PrNumber, label: &str) -> Result<(), ApiError> {
        let path = format!("/issues/{}/labels/{}", number.0, encode(label));
        let response = self.call(Method::Delete, &path, None)?;
        match response.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(failure(Method::Delete, &path, &response)),
        }
    }

    fn list_comments(&mut self, number: PrNumber) -> Result<Vec<String>, ApiError> {
        let path = format!("/issues/{}/comments", number.0);
        let comments: Vec<CommentJson> = self.get_array(&path)?;
        Ok(comments.into_iter().map(|c| c.body).collect())
    }

    fn create_comment(&mut self, number: PrNumber, body: &str) -> Result<(), ApiError> {
        let path = format!("/issues/{}/comments", number.0);
        self.json_call(Method::Post, &path, Some(&json!({ "body": body })))
            .map(|_| ())
    }

    fn get_issue(&mut self, number: u64) -> Result<Option<IssueDetail>, ApiError> {
        let path = format!("/issues/{number}");
        let response = self.call(Method::Get, &path, None)?;
        match response.status {
            200 => {
                let what = format!("GET {path}");
                let issue: IssueJson = decode(&what, response.json(&what)?)?;
                Ok(Some(IssueDetail {
                    pull_request: issue.pull_request.is_some(),
                }))
            }
            404 => Ok(None),
            _ => Err(issue_failure(Method::Get, &path, &response)),
        }
    }

    fn set_issue_body(&mut self, number: u64, body: &str) -> Result<(), ApiError> {
        let path = format!("/issues/{number}");
        let response = self.call(Method::Patch, &path, Some(&json!({ "body": body })))?;
        match response.status {
            200 => Ok(()),
            _ => Err(issue_failure(Method::Patch, &path, &response)),
        }
    }

    fn label_exists(&mut self, label: &str) -> Result<bool, ApiError> {
        let path = format!("/labels/{}", encode(label));
        let response = self.call(Method::Get, &path, None)?;
        match response.status {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(failure(Method::Get, &path, &response)),
        }
    }

    fn find_ruleset(
        &mut self,
        name: &str,
    ) -> Result<Option<(RulesetId, BTreeSet<String>)>, ApiError> {
        let path = "/rulesets?includes_parents=false";
        let mut found = Vec::new();
        for page in self.get_pages(path, admin_failure)? {
            let listed: Vec<RulesetSummaryJson> = decode(&format!("GET {path}"), page)?;
            found.extend(listed.into_iter().filter(|r| r.name == name).map(|r| r.id));
        }
        let id = match found[..] {
            [] => return Ok(None),
            [id] => id,
            _ => {
                return Err(permanent(format!(
                    "GET {path}: {} rulesets are named {name:?}; the queue owns one, \
                     delete the others",
                    found.len()
                )));
            }
        };
        let path = format!("/rulesets/{id}");
        let what = format!("GET {path}");
        let response = self.call(Method::Get, &path, None)?;
        if response.status != 200 {
            return Err(admin_failure(Method::Get, &path, &response));
        }
        let ruleset: RulesetJson = decode(&what, response.json(&what)?)?;
        let branches = ruleset
            .conditions
            .ref_name
            .include
            .iter()
            .map(|name| {
                name.strip_prefix("refs/heads/")
                    .map(str::to_owned)
                    .ok_or_else(|| permanent(format!("{what}: {name:?} is not a branch")))
            })
            .collect::<Result<_, _>>()?;
        Ok(Some((RulesetId(id), branches)))
    }

    fn create_ruleset(
        &mut self,
        name: &str,
        branches: &BTreeSet<String>,
    ) -> Result<RulesetId, ApiError> {
        let path = "/rulesets";
        let what = format!("POST {path}");
        let mut body = ruleset_body(branches);
        body["name"] = json!(name);
        let response = self.call(Method::Post, path, Some(&body))?;
        if response.status != 201 {
            return Err(admin_failure(Method::Post, path, &response));
        }
        let created: RulesetSummaryJson = decode(&what, response.json(&what)?)?;
        Ok(RulesetId(created.id))
    }

    fn update_ruleset(
        &mut self,
        id: RulesetId,
        branches: &BTreeSet<String>,
    ) -> Result<bool, ApiError> {
        let path = format!("/rulesets/{}", id.0);
        let response = self.call(Method::Put, &path, Some(&ruleset_body(branches)))?;
        match response.status {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(admin_failure(Method::Put, &path, &response)),
        }
    }

    fn delete_ruleset(&mut self, id: RulesetId) -> Result<(), ApiError> {
        let path = format!("/rulesets/{}", id.0);
        let response = self.call(Method::Delete, &path, None)?;
        match response.status {
            204 | 404 => Ok(()),
            _ => Err(admin_failure(Method::Delete, &path, &response)),
        }
    }

    fn list_runs(&mut self, head: &Sha) -> Result<Vec<WorkflowRun>, ApiError> {
        let path = format!("/actions/runs?head_sha={head}");
        let what = format!("GET {path}");
        let mut runs = Vec::new();
        for page in self.get_pages(&path, actions_failure)? {
            let listed: WorkflowRunsJson = decode(&what, page)?;
            runs.extend(listed.workflow_runs.into_iter().map(|run| WorkflowRun {
                id: RunId(run.id),
                head_branch: run.head_branch,
                completed: run.status.as_deref() == Some("completed"),
            }));
        }
        Ok(runs)
    }

    fn cancel_run(&mut self, id: RunId) -> Result<(), ApiError> {
        let path = format!("/actions/runs/{}/cancel", id.0);
        let response = self.call(Method::Post, &path, None)?;
        match response.status {
            // 409: the run has already completed.
            202 | 404 | 409 => Ok(()),
            _ => Err(actions_failure(Method::Post, &path, &response)),
        }
    }

    fn rate_limit_pause(&mut self) -> Option<SystemTime> {
        self.paused_until.take()
    }
}

/// The body of the queue's ruleset, less its name: an active branch
/// ruleset that refuses every update of `branches` to everyone.
fn ruleset_body(branches: &BTreeSet<String>) -> Value {
    let include: Vec<String> = branches.iter().map(|b| crate::branch(b)).collect();
    json!({
        "target": "branch",
        "enforcement": "active",
        "bypass_actors": [],
        "conditions": { "ref_name": { "include": include, "exclude": [] } },
        "rules": [{ "type": "update" }],
    })
}

/// [`failure`], naming the permission a ruleset call needs when GitHub
/// refuses it for good with a `403`.
fn admin_failure(method: Method, path: &str, response: &HttpResponse) -> ApiError {
    permission_failure(
        method,
        path,
        response,
        "the Administration permission: read/write, for the queue's branch-lock ruleset",
    )
}

/// [`failure`], naming the permission a workflow-run call needs when GitHub
/// refuses it for good with a `403`.
fn actions_failure(method: Method, path: &str, response: &HttpResponse) -> ApiError {
    permission_failure(
        method,
        path,
        response,
        "the Actions permission: read/write, for cancelling a retired batch's workflow runs",
    )
}

/// [`failure`], with "the credential needs `needs`" appended to a
/// permanent `403`; a rate-limited `403` stays transient.
fn permission_failure(
    method: Method,
    path: &str,
    response: &HttpResponse,
    needs: &str,
) -> ApiError {
    match failure(method, path, response) {
        ApiError::Permanent(e) if response.status == 403 => {
            permanent(format!("{e} (the credential needs {needs})"))
        }
        error => error,
    }
}

fn check_run_outcome(what: &str, run: &CheckRunJson) -> Result<CheckOutcome, ApiError> {
    if run.status != "completed" {
        return Ok(CheckOutcome::Pending);
    }
    match run.conclusion.as_deref() {
        Some("success" | "neutral" | "skipped") => Ok(CheckOutcome::Passed),
        Some(_) => Ok(CheckOutcome::Failed),
        None => Err(permanent(format!(
            "{what}: completed check run {:?} has no conclusion",
            run.name
        ))),
    }
}

/// What every request passes through: the transport, the clock, and where
/// a rate-limited response's pause is recorded.
type Exchange<'a> = (&'a mut Transport, &'a Clock, &'a mut Option<SystemTime>);

/// One request through the transport; any status is returned. A
/// rate-limited response records its pause, kept if later than one already
/// recorded.
fn exchange(
    (transport, clock, paused_until): Exchange<'_>,
    method: Method,
    path: &str,
    url: &str,
    authorization: &str,
    body: Option<&Value>,
) -> Result<HttpResponse, ApiError> {
    let request = HttpRequest {
        method,
        url,
        authorization,
        body,
    };
    let response = transport(&request).map_err(|e| {
        let message = format!("{method} {path}: {}", e.message);
        if e.transient {
            ApiError::Transient(message.into())
        } else {
            ApiError::Permanent(message.into())
        }
    })?;
    if let Some(limit) = rate_limit(&response) {
        let until = limit.until(clock());
        *paused_until = Some(paused_until.map_or(until, |earlier| earlier.max(until)));
    }
    Ok(response)
}

/// One request authorised by the app's JWT, decoded from a 2xx response.
/// Every error says what was being `doing`.
fn app_call<T: for<'de> Deserialize<'de>>(
    through: Exchange<'_>,
    doing: &str,
    method: Method,
    path: &str,
    url: &str,
    jwt: &str,
    body: Option<&Value>,
) -> Result<T, ApiError> {
    let in_context = |error: ApiError| match error {
        ApiError::Transient(e) => ApiError::Transient(format!("{doing}: {e}").into()),
        ApiError::Permanent(e) => ApiError::Permanent(format!("{doing}: {e}").into()),
    };
    let response = exchange(through, method, path, url, jwt, body).map_err(in_context)?;
    if response.status == 401 {
        return Err(permanent(format!(
            "{doing}: {method} {path}: HTTP 401: {} ({JWT_REJECTED})",
            github_message(&response)
        )));
    }
    if !(200..300).contains(&response.status) {
        return Err(in_context(failure(method, path, &response)));
    }
    let what = format!("{doing}: {method} {path}");
    decode(&what, response.json(&what)?)
}

/// The error a response with an unexpected status maps to.
fn failure(method: Method, path: &str, response: &HttpResponse) -> ApiError {
    let status = response.status;
    let detail = github_message(response);
    let message = format!("{method} {path}: HTTP {status}: {detail}");
    if let Some(limit) = rate_limit(response) {
        let until = match limit {
            RateLimit::After(seconds) => format!("retry after {seconds} s"),
            RateLimit::ResetAt(reset) => format!("limit resets at unix time {reset}"),
            RateLimit::Unstated => format!("retry after at least {UNSTATED_PAUSE_SECS} s"),
        };
        return ApiError::Transient(format!("{message} (rate limited; {until})").into());
    }
    if (300..400).contains(&status) {
        let location = response.header("location").unwrap_or("no location given");
        return permanent(format!(
            "{message} (redirected to {location}; if the repository was renamed or \
             transferred, update owner and repo)"
        ));
    }
    if status >= 500 {
        return ApiError::Transient(message.into());
    }
    permanent(message)
}

/// [`failure`] for a call on one issue: a redirect there also means the
/// issue was transferred to another repository.
fn issue_failure(method: Method, path: &str, response: &HttpResponse) -> ApiError {
    match failure(method, path, response) {
        ApiError::Permanent(error) if (300..400).contains(&response.status) => permanent(format!(
            "{error}; if the issue was transferred, set status_issue to an issue in this \
             repository"
        )),
        error => error,
    }
}

/// How long a rate limit that names no time pauses the queue.
const UNSTATED_PAUSE_SECS: u64 = 60;

/// When a rate-limited response says requests may resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RateLimit {
    /// `retry-after`: this many seconds from now.
    After(u64),
    /// `x-ratelimit-reset` with `x-ratelimit-remaining` at `0`: this unix
    /// time.
    ResetAt(u64),
    /// No `retry-after` and the primary limit not exhausted, or the header
    /// that would name the time not readable as whole seconds: a secondary
    /// limit known only by its status or message.
    Unstated,
}

impl RateLimit {
    fn until(self, now: SystemTime) -> SystemTime {
        match self {
            RateLimit::After(seconds) => now + Duration::from_secs(seconds),
            RateLimit::ResetAt(reset) => UNIX_EPOCH + Duration::from_secs(reset),
            RateLimit::Unstated => now + Duration::from_secs(UNSTATED_PAUSE_SECS),
        }
    }
}

/// The rate limit a response reports: a `429`, or a `403` with the
/// primary limit exhausted, a `retry-after`, or a secondary limit's
/// message. `None` for any other response.
fn rate_limit(response: &HttpResponse) -> Option<RateLimit> {
    let status = response.status;
    // A secondary rate limit may carry neither header; its message says
    // what it is.
    let rate_limited = status == 429
        || status == 403
            && (response.header("x-ratelimit-remaining") == Some("0")
                || response.header("retry-after").is_some()
                || github_message(response)
                    .to_ascii_lowercase()
                    .contains("secondary rate limit"));
    if !rate_limited {
        return None;
    }
    let seconds = |name| {
        response
            .header(name)
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    // Every response carries `x-ratelimit-reset`, the primary window's end;
    // it says when requests may resume only once that window is exhausted.
    let exhausted = response.header("x-ratelimit-remaining") == Some("0");
    Some(
        match (seconds("retry-after"), seconds("x-ratelimit-reset")) {
            (Some(after), _) => RateLimit::After(after),
            (None, Some(reset)) if exhausted => RateLimit::ResetAt(reset),
            (None, _) => RateLimit::Unstated,
        },
    )
}

/// GitHub's `message` field from an error body followed by the detail of each
/// of its `errors`, or the body itself.
fn github_message(response: &HttpResponse) -> String {
    #[derive(Deserialize)]
    struct ErrorJson {
        message: String,
        #[serde(default)]
        errors: Vec<serde_json::Value>,
    }
    let e = match serde_json::from_str::<ErrorJson>(&response.body) {
        Ok(e) => e,
        Err(_) => return response.body.chars().take(200).collect(),
    };
    let details: Vec<&str> = e
        .errors
        .iter()
        .filter_map(|error| {
            error
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| error.get("code").and_then(|c| c.as_str()))
                .or_else(|| error.as_str())
        })
        .collect();
    if details.is_empty() {
        e.message
    } else {
        format!("{}: {}", e.message, details.join("; "))
    }
}

/// The `rel="next"` URL of a `Link` header.
fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, params) = part.trim().split_once(';')?;
        let is_next = params.split(';').any(|p| p.trim() == "rel=\"next\"");
        let url = url.trim().strip_prefix('<')?.strip_suffix('>')?;
        is_next.then(|| url.to_owned())
    })
}

/// A full ref name with `refs/` removed and each segment encoded, as the
/// git refs endpoints take it in their path.
fn ref_path(name: &str) -> Result<String, ApiError> {
    let rest = name
        .strip_prefix("refs/")
        .ok_or_else(|| permanent(format!("ref name {name:?} does not start with refs/")))?;
    Ok(rest.split('/').map(encode).collect::<Vec<_>>().join("/"))
}

/// Percent-encodes everything but RFC 3986's unreserved characters.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode<T: for<'de> Deserialize<'de>>(what: &str, value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value)
        .map_err(|e| permanent(format!("{what}: unexpected response: {e}")))
}

fn parse_sha(what: &str, text: &str) -> Result<Sha, ApiError> {
    Sha::parse(text).map_err(|e| permanent(format!("{what}: {e}")))
}

/// Sends one request through ureq; the status is never an error here.
fn send(agent: &ureq::Agent, request: &HttpRequest<'_>) -> Result<HttpResponse, TransportError> {
    let authorization = request.authorization;
    fn headers<B>(
        builder: ureq::RequestBuilder<B>,
        authorization: &str,
    ) -> ureq::RequestBuilder<B> {
        builder
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Authorization", authorization)
    }
    // A request with no body sends none, rather than a JSON `null`.
    let send_body = |builder: ureq::RequestBuilder<ureq::typestate::WithBody>| {
        let builder = headers(builder, authorization);
        match request.body {
            Some(body) => builder.send_json(body),
            None => builder.send_empty(),
        }
    };
    let result = match request.method {
        Method::Get => headers(agent.get(request.url), authorization).call(),
        Method::Delete => headers(agent.delete(request.url), authorization).call(),
        Method::Post => send_body(agent.post(request.url)),
        Method::Patch => send_body(agent.patch(request.url)),
        Method::Put => send_body(agent.put(request.url)),
    };
    let mut response = result.map_err(transport_error)?;
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((
                name.as_str().to_ascii_lowercase(),
                value.to_str().ok()?.to_owned(),
            ))
        })
        .collect();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(transport_error)?;
    Ok(HttpResponse {
        status: response.status().as_u16(),
        headers,
        body,
    })
}

fn transport_error(error: ureq::Error) -> TransportError {
    let transient = matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
            | ureq::Error::Protocol(_)
    );
    TransportError {
        transient,
        message: error.to_string(),
    }
}

#[derive(Deserialize)]
struct InstallationJson {
    id: u64,
}

#[derive(Deserialize)]
struct AccessTokenJson {
    token: String,
}

#[derive(Deserialize)]
struct ShaJson {
    sha: String,
}

#[derive(Deserialize)]
struct RefJson {
    #[serde(rename = "ref")]
    name: String,
    object: ShaJson,
}

#[derive(Deserialize)]
struct CommitJson {
    message: String,
    tree: ShaJson,
    parents: Vec<ShaJson>,
}

#[derive(Deserialize)]
struct CompareJson {
    status: String,
    ahead_by: u32,
    merge_base_commit: ShaJson,
}

/// A compare response as a [`Compare`]; an unknown status is permanent.
fn compare_of(what: &str, compared: CompareJson) -> Result<Compare, ApiError> {
    let status = match compared.status.as_str() {
        "identical" => CompareStatus::Identical,
        "ahead" => CompareStatus::Ahead,
        "behind" => CompareStatus::Behind,
        "diverged" => CompareStatus::Diverged,
        other => {
            return Err(permanent(format!(
                "{what}: unknown compare status {other:?}"
            )));
        }
    };
    Ok(Compare {
        status,
        ahead_by: compared.ahead_by,
        merge_base: parse_sha(what, &compared.merge_base_commit.sha)?,
    })
}

#[derive(Deserialize)]
struct LabelJson {
    name: String,
}

#[derive(Deserialize)]
struct BranchJson {
    #[serde(rename = "ref")]
    branch: String,
}

#[derive(Deserialize)]
struct RepoJson {
    full_name: String,
}

#[derive(Deserialize)]
struct HeadJson {
    sha: String,
    #[serde(rename = "ref")]
    branch: String,
    /// Null for a head in a deleted fork.
    repo: Option<RepoJson>,
}

#[derive(Deserialize)]
struct IssueJson {
    number: u64,
    /// Present only on an issue that is a pull request.
    #[serde(default)]
    pull_request: Option<Value>,
}

#[derive(Deserialize)]
struct PrJson {
    number: u64,
    title: String,
    state: String,
    head: HeadJson,
    base: BranchJson,
    labels: Vec<LabelJson>,
    /// Only the single-item endpoint carries it.
    #[serde(default)]
    mergeable_state: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    /// Only the single-item endpoint carries it.
    #[serde(default)]
    merged: Option<bool>,
    /// The list endpoint's merge record; null when not merged.
    #[serde(default)]
    merged_at: Option<String>,
}

#[derive(Deserialize)]
struct CheckRunsJson {
    check_runs: Vec<CheckRunJson>,
}

#[derive(Deserialize)]
struct CheckRunJson {
    name: String,
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Deserialize)]
struct CombinedStatusJson {
    statuses: Vec<StatusJson>,
}

#[derive(Deserialize)]
struct StatusJson {
    context: String,
    state: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct RulesetSummaryJson {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct RulesetJson {
    conditions: RulesetConditionsJson,
}

#[derive(Deserialize)]
struct RulesetConditionsJson {
    ref_name: RefNameConditionJson,
}

#[derive(Deserialize)]
struct RefNameConditionJson {
    include: Vec<String>,
}

#[derive(Deserialize)]
struct WorkflowRunsJson {
    workflow_runs: Vec<WorkflowRunJson>,
}

#[derive(Deserialize)]
struct WorkflowRunJson {
    id: u64,
    head_branch: Option<String>,
    status: Option<String>,
}

#[derive(Deserialize)]
struct CommentJson {
    body: String,
}

#[cfg(test)]
mod tests;
