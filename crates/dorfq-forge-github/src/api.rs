//! The GitHub REST surface the forge uses: the [`GitHubApi`] trait, its
//! typed results, and [`RestClient`], which implements it over HTTPS.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use dorfq_core::{ForgeError, PrNumber, Sha};
use serde::Deserialize;
use serde_json::{Value, json};

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
    /// Open pull requests targeting the branch `base`.
    fn list_open_prs(&mut self, base: &str) -> Result<Vec<PrSummary>, ApiError>;
    fn get_pr(&mut self, number: PrNumber) -> Result<PrDetail, ApiError>;
    /// Check runs and commit statuses on `sha`, newest first.
    fn checks(&mut self, sha: &Sha) -> Result<Vec<CheckResult>, ApiError>;
    /// Removes a label from a pull request; an absent label is not an error.
    fn remove_label(&mut self, number: PrNumber, label: &str) -> Result<(), ApiError>;
    /// Comment bodies on a pull request, oldest first.
    fn list_comments(&mut self, number: PrNumber) -> Result<Vec<String>, ApiError>;
    fn create_comment(&mut self, number: PrNumber, body: &str) -> Result<(), ApiError>;
    fn label_exists(&mut self, label: &str) -> Result<bool, ApiError>;
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

/// An open pull request, as the list endpoint reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    pub number: PrNumber,
    pub head: Sha,
    pub labels: Vec<String>,
    pub title: String,
}

/// One pull request, as the single-item endpoint reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrDetail {
    pub open: bool,
    /// The branch it targets.
    pub base: String,
    pub head: Sha,
    pub labels: Vec<String>,
    /// `None` while GitHub has not computed it.
    pub mergeable: Option<bool>,
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
    Delete,
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        })
    }
}

struct HttpRequest<'a> {
    method: Method,
    url: &'a str,
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

/// [`GitHubApi`] over GitHub's REST API (github.com or GitHub Enterprise).
pub struct RestClient {
    /// `{api_base}/repos/{owner}/{repo}`.
    repo_url: String,
    transport: Transport,
}

const PER_PAGE: &str = "per_page=100";

impl RestClient {
    /// A client for one repository. `api_base` is `https://api.github.com`
    /// or a GitHub Enterprise `https://<host>/api/v3`; `token` is sent as a
    /// bearer token on every request.
    pub fn new(
        api_base: &str,
        owner: &str,
        repo: &str,
        token: &str,
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
        let authorization = format!("Bearer {token}");
        let transport: Transport =
            Box::new(move |request: &HttpRequest<'_>| send(&agent, &authorization, request));
        RestClient::with_transport(api_base, owner, repo, transport)
    }

    fn with_transport(api_base: &str, owner: &str, repo: &str, transport: Transport) -> RestClient {
        RestClient {
            repo_url: format!(
                "{}/repos/{}/{}",
                api_base.trim_end_matches('/'),
                encode(owner),
                encode(repo)
            ),
            transport,
        }
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
        let url = format!("{}{path}", self.repo_url);
        self.call_url(method, path, &url, body)
    }

    fn call_url(
        &mut self,
        method: Method,
        path: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<HttpResponse, ApiError> {
        let request = HttpRequest { method, url, body };
        (self.transport)(&request).map_err(|e| {
            let message = format!("{method} {path}: {}", e.message);
            if e.transient {
                ApiError::Transient(message.into())
            } else {
                ApiError::Permanent(message.into())
            }
        })
    }

    /// Every page of a list endpoint, following `Link: rel="next"`.
    fn get_pages(&mut self, path: &str) -> Result<Vec<Value>, ApiError> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut url = format!("{}{path}{separator}{PER_PAGE}", self.repo_url);
        let mut pages = Vec::new();
        loop {
            let response = self.call_url(Method::Get, path, &url, None)?;
            if response.status != 200 {
                return Err(failure(Method::Get, path, &response));
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
        for page in self.get_pages(path)? {
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
        let path = format!("/compare/{ancestor}...{descendant}");
        let what = format!("GET {path}");
        let compared: CompareJson = decode(&what, self.json_call(Method::Get, &path, None)?)?;
        match compared.status.as_str() {
            "identical" | "ahead" => Ok(true),
            "behind" | "diverged" => Ok(false),
            other => Err(permanent(format!(
                "{what}: unknown compare status {other:?}"
            ))),
        }
    }

    fn list_open_prs(&mut self, base: &str) -> Result<Vec<PrSummary>, ApiError> {
        let path = format!("/pulls?state=open&base={}", encode(base));
        let what = format!("GET {path}");
        let found: Vec<PrJson> = self.get_array(&path)?;
        found
            .into_iter()
            .map(|pr| {
                Ok(PrSummary {
                    number: PrNumber(pr.number),
                    head: parse_sha(&what, &pr.head.sha)?,
                    labels: pr.labels.into_iter().map(|l| l.name).collect(),
                    title: pr.title,
                })
            })
            .collect()
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
        Ok(PrDetail {
            open,
            base: pr.base.branch,
            head: parse_sha(&what, &pr.head.sha)?,
            labels: pr.labels.into_iter().map(|l| l.name).collect(),
            mergeable: pr.mergeable,
        })
    }

    fn checks(&mut self, sha: &Sha) -> Result<Vec<CheckResult>, ApiError> {
        let mut timed: Vec<(Option<String>, CheckResult)> = Vec::new();

        let path = format!("/commits/{sha}/check-runs");
        let what = format!("GET {path}");
        for page in self.get_pages(&path)? {
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
        for page in self.get_pages(&path)? {
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

    fn label_exists(&mut self, label: &str) -> Result<bool, ApiError> {
        let path = format!("/labels/{}", encode(label));
        let response = self.call(Method::Get, &path, None)?;
        match response.status {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(failure(Method::Get, &path, &response)),
        }
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

/// The error a response with an unexpected status maps to.
fn failure(method: Method, path: &str, response: &HttpResponse) -> ApiError {
    let status = response.status;
    let detail = github_message(response);
    let message = format!("{method} {path}: HTTP {status}: {detail}");
    // A secondary rate limit may carry neither header; its message says
    // what it is.
    let rate_limited = response.header("x-ratelimit-remaining") == Some("0")
        || response.header("retry-after").is_some()
        || detail.to_ascii_lowercase().contains("secondary rate limit");
    if status == 429 || (status == 403 && rate_limited) {
        let until = match (
            response.header("retry-after"),
            response.header("x-ratelimit-reset"),
        ) {
            (Some(seconds), _) => format!("retry after {seconds} s"),
            (None, Some(reset)) => format!("limit resets at unix time {reset}"),
            (None, None) => "retry after at least 60 s".to_owned(),
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

/// GitHub's `message` field from an error body, or the body itself.
fn github_message(response: &HttpResponse) -> String {
    #[derive(Deserialize)]
    struct ErrorJson {
        message: String,
    }
    match serde_json::from_str::<ErrorJson>(&response.body) {
        Ok(e) => e.message,
        Err(_) => response.body.chars().take(200).collect(),
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
fn send(
    agent: &ureq::Agent,
    authorization: &str,
    request: &HttpRequest<'_>,
) -> Result<HttpResponse, TransportError> {
    fn headers<B>(
        builder: ureq::RequestBuilder<B>,
        authorization: &str,
    ) -> ureq::RequestBuilder<B> {
        builder
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Authorization", authorization)
    }
    let empty = Value::Null;
    let result = match request.method {
        Method::Get => headers(agent.get(request.url), authorization).call(),
        Method::Delete => headers(agent.delete(request.url), authorization).call(),
        Method::Post => headers(agent.post(request.url), authorization)
            .send_json(request.body.unwrap_or(&empty)),
        Method::Patch => headers(agent.patch(request.url), authorization)
            .send_json(request.body.unwrap_or(&empty)),
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
struct PrJson {
    number: u64,
    title: String,
    state: String,
    head: ShaJson,
    base: BranchJson,
    labels: Vec<LabelJson>,
    #[serde(default)]
    mergeable: Option<bool>,
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
struct CommentJson {
    body: String,
}

#[cfg(test)]
mod tests;
