//! Builds the forge and the scheduler a configuration names.

use std::cell::RefCell;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};

use dorfq_core::{
    BatchId, BatchStart, EjectReason, Forge, ForgeError, NoticeReason, Observation, PrNumber,
    Scheduler, Sha,
};
use dorfq_forge_github::api::{ApiError, Auth, ChangedFile, GitHubApi, RestClient};
use dorfq_forge_github::app::AppKey;
use dorfq_forge_github::webhook::{Hint, Listener};
use dorfq_forge_github::{GitHubForge, HeadFiles, Settings, system_clock};
use dorfq_forge_mock::{MockForge, Scenario, ScenarioError};
use dorfq_scheduler::SchedulerConfig;

use crate::config::{AuthConfig, ForgeConfig, WebhookConfig};
use crate::journal::capture::Reader;

/// A provider that could not be built.
#[derive(Debug)]
pub enum ProviderError {
    /// The mock's scenario file could not be read.
    ReadScenario { path: PathBuf, error: io::Error },
    /// The mock's scenario file is invalid.
    Scenario { path: PathBuf, error: ScenarioError },
    /// The environment variable that should hold the GitHub token is unset,
    /// empty or not UTF-8.
    MissingToken { var: String },
    /// The environment variable that should hold the webhook secret is
    /// unset, empty or not UTF-8.
    MissingWebhookSecret { var: String },
    /// The webhook receiver could not bind its address.
    Listen {
        address: SocketAddr,
        error: io::Error,
    },
    /// The GitHub App's private key file could not be read.
    ReadPrivateKey { path: PathBuf, error: io::Error },
    /// The GitHub App's private key file is not a usable key; the message
    /// says why.
    PrivateKey { path: PathBuf, message: String },
    /// The GitHub forge's startup checks failed.
    GitHub(ApiError),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::ReadScenario { path, error } => {
                write!(f, "cannot read scenario {}: {error}", path.display())
            }
            ProviderError::Scenario { path, error } => {
                write!(f, "invalid scenario {}: {error}", path.display())
            }
            ProviderError::MissingToken { var } => write!(
                f,
                "environment variable {var}, which should hold the GitHub token, \
                 is unset, empty or not UTF-8"
            ),
            ProviderError::MissingWebhookSecret { var } => write!(
                f,
                "environment variable {var}, which should hold the webhook secret, \
                 is unset, empty or not UTF-8"
            ),
            ProviderError::Listen { error, .. } => error.fmt(f),
            ProviderError::ReadPrivateKey { path, error } => write!(
                f,
                "cannot read the GitHub App private key {}: {error}",
                path.display()
            ),
            ProviderError::PrivateKey { path, message } => write!(
                f,
                "the GitHub App private key {} is not usable: {message}",
                path.display()
            ),
            ProviderError::GitHub(error) => write!(f, "cannot start the GitHub forge: {error}"),
        }
    }
}

impl Error for ProviderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ProviderError::ReadScenario { error, .. } => Some(error),
            ProviderError::Scenario { error, .. } => Some(error),
            ProviderError::MissingToken { .. } => None,
            ProviderError::MissingWebhookSecret { .. } => None,
            ProviderError::Listen { error, .. } => Some(error),
            ProviderError::ReadPrivateKey { error, .. } => Some(error),
            ProviderError::PrivateKey { .. } => None,
            ProviderError::GitHub(error) => Some(error),
        }
    }
}

/// A forge the scaffold built, and what the scaffold knows about it beyond
/// the contract.
pub struct BuiltForge {
    pub forge: Box<dyn Forge>,
    /// True while the forge will still produce events on its own: a
    /// scenario's pull requests that have not arrived. A forge whose events
    /// come from the world never has any.
    pub has_scheduled_events: Box<dyn Fn() -> bool>,
    /// Blocks until the next tick is due, given how the last one ended.
    pub wait: Box<dyn FnMut(Outcome)>,
    /// Hands the forge a new status body to write to the status issue:
    /// `Some` for the GitHub forge with `status_issue` set, `None` otherwise.
    pub status: Option<Box<dyn FnMut(String)>>,
    /// Reads what the forge knows that no observation carries: `Some` for
    /// the GitHub forge, `None` otherwise.
    pub gauges: Option<Box<dyn FnMut() -> ForgeGauges>>,
    /// Reads what a pull request's head changes, for the capture file.
    pub capture: Box<Reader<'static>>,
}

/// What the GitHub forge knows that no observation carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgeGauges {
    /// Labelled but not enqueued at the latest observation.
    pub blocked_prs: u64,
    /// The last `x-ratelimit-remaining` GitHub reported; `None` when unknown.
    pub rate_limit_remaining: Option<u64>,
}

/// How a tick ended, for [`BuiltForge::wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It ran to the end, or failed for good.
    Done,
    /// A forge call failed transiently; the tick is to be retried.
    Transient,
}

/// A forge behind a [`Forge`], still reachable through the other handles on
/// it.
pub(crate) struct Shared<F>(pub(crate) Rc<RefCell<F>>);

impl<F: Forge> Forge for Shared<F> {
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        self.0.borrow_mut().observe()
    }

    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.0.borrow_mut().start_batch(batch)
    }

    fn land(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.0.borrow_mut().land(batch)
    }

    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.0.borrow_mut().retire(batch)
    }

    fn eject(&mut self, pr: PrNumber, reason: &EjectReason) -> Result<(), ForgeError> {
        self.0.borrow_mut().eject(pr, reason)
    }

    fn notify(&mut self, pr: PrNumber, reason: &NoticeReason) -> Result<(), ForgeError> {
        self.0.borrow_mut().notify(pr, reason)
    }
}

/// Builds the configured forge. Either forge writes its messages to stdout.
/// The mock's `wait` returns at once; the GitHub forge's waits on its
/// refresh schedule and, with a webhook configured, its receiver's hints.
pub fn forge(config: &ForgeConfig) -> Result<BuiltForge, ProviderError> {
    match config {
        ForgeConfig::Mock { scenario } => {
            let text =
                fs::read_to_string(scenario).map_err(|error| ProviderError::ReadScenario {
                    path: scenario.clone(),
                    error,
                })?;
            let parsed = Scenario::from_toml(&text).map_err(|error| ProviderError::Scenario {
                path: scenario.clone(),
                error,
            })?;
            let mock = Rc::new(RefCell::new(MockForge::new(parsed, Box::new(io::stdout()))));
            let forge = Shared(Rc::clone(&mock));
            Ok(BuiltForge {
                forge: Box::new(forge),
                has_scheduled_events: Box::new(move || mock.borrow().awaiting_arrivals()),
                wait: Box::new(|_| {}),
                status: None,
                gauges: None,
                capture: Box::new(mock_head_files),
            })
        }
        ForgeConfig::GitHub {
            api_base,
            owner,
            repo,
            auth,
            webhook,
            ..
        } => {
            let secret = webhook.as_ref().map(webhook_secret).transpose()?;
            let auth = github_auth(auth)?;
            let client = RestClient::new(api_base, owner, repo, auth, &crate::user_agent());
            let settings = config
                .github_settings()
                .expect("a GitHub forge configuration has GitHub settings");
            let hints = match webhook.as_ref().zip(secret) {
                Some((webhook, secret)) => {
                    let repository = format!("{owner}/{repo}");
                    Some(listen(webhook.listen, secret, repository, &settings)?.1)
                }
                None => None,
            };
            github(Box::new(client), settings, hints)
        }
    }
}

/// Builds the GitHub forge over `api` and the handles on it.
fn github(
    api: Box<dyn GitHubApi>,
    settings: Settings,
    hints: Option<Receiver<Hint>>,
) -> Result<BuiltForge, ProviderError> {
    let status_issue = settings.status_issue;
    let forge = GitHubForge::new(
        api,
        settings,
        Box::new(system_clock),
        Box::new(io::stdout()),
        hints,
    )
    .map_err(ProviderError::GitHub)?;
    let forge = Rc::new(RefCell::new(forge));
    let waiting = Rc::clone(&forge);
    let reading = Rc::clone(&forge);
    let gauges = Box::new(move || {
        let forge = reading.borrow();
        ForgeGauges {
            blocked_prs: forge.blocked_prs() as u64,
            rate_limit_remaining: forge.rate_limit_remaining(),
        }
    }) as Box<dyn FnMut() -> ForgeGauges>;
    let status = status_issue.map(|_| {
        let publishing = Rc::clone(&forge);
        Box::new(move |body| publishing.borrow_mut().publish_status(body)) as Box<dyn FnMut(String)>
    });
    let capturing = Rc::clone(&forge);
    let capture = Box::new(move |pr, head: &Sha| {
        capturing
            .borrow_mut()
            .head_files(pr, head)
            .map_err(ForgeError::from)
    }) as Box<Reader<'static>>;
    Ok(BuiltForge {
        forge: Box::new(Shared(forge)),
        has_scheduled_events: Box::new(|| false),
        wait: Box::new(move |outcome| waiting.borrow_mut().wait(outcome == Outcome::Transient)),
        status,
        gauges: Some(gauges),
        capture,
    })
}

/// The mock forge's answer for any pull request at any head: one file,
/// `pr/<n>`, one line added, under an all-zero base.
fn mock_head_files(pr: PrNumber, _: &Sha) -> Result<Result<HeadFiles, Sha>, ForgeError> {
    Ok(Ok(HeadFiles {
        base: Sha::parse(&"0".repeat(40)).expect("forty zeros are a sha"),
        additions: 1,
        deletions: 0,
        changed_files: 1,
        files: vec![ChangedFile {
            path: format!("pr/{}", pr.0),
            additions: 1,
            deletions: 0,
            status: "modified".to_owned(),
            previous: None,
        }],
    }))
}

/// The webhook secret from the environment variable `webhook` names.
fn webhook_secret(webhook: &WebhookConfig) -> Result<String, ProviderError> {
    env_secret(&webhook.secret_env).ok_or_else(|| ProviderError::MissingWebhookSecret {
        var: webhook.secret_env.clone(),
    })
}

/// The value of environment variable `var`; `None` when it is unset, empty
/// or not UTF-8.
pub(crate) fn env_secret(var: &str) -> Option<String> {
    env::var(var).ok().filter(|value| !value.is_empty())
}

/// Starts the webhook receiver on `address`, filtering by `repository`
/// (`owner/repo`) and `settings`; the receiver and the channel its hints
/// arrive on. Writes a stderr line naming the address it listens on.
fn listen(
    address: SocketAddr,
    secret: String,
    repository: String,
    settings: &Settings,
) -> Result<(Listener, Receiver<Hint>), ProviderError> {
    let (send, receive) = mpsc::channel();
    let listener = Listener::spawn(address, secret, repository, settings.clone(), send)
        .map_err(|error| ProviderError::Listen { address, error })?;
    eprintln!(
        "dorfq: webhook receiver listening on {}",
        listener.address()
    );
    Ok((listener, receive))
}

/// Loads the credential the configuration names: the token from its
/// environment variable, or the app's key from its file.
fn github_auth(config: &AuthConfig) -> Result<Auth, ProviderError> {
    match config {
        AuthConfig::Token { env: var } => env_secret(var)
            .map(Auth::Token)
            .ok_or_else(|| ProviderError::MissingToken { var: var.clone() }),
        AuthConfig::App {
            client_id,
            private_key_file,
        } => {
            let pem = fs::read_to_string(private_key_file).map_err(|error| {
                ProviderError::ReadPrivateKey {
                    path: private_key_file.clone(),
                    error,
                }
            })?;
            let key =
                AppKey::from_pem(client_id, &pem).map_err(|message| ProviderError::PrivateKey {
                    path: private_key_file.clone(),
                    message,
                })?;
            Ok(Auth::App(key))
        }
    }
}

/// Builds the configured scheduler, whose parameters
/// [`crate::config::Config`] validated at load.
pub fn scheduler(config: &SchedulerConfig) -> Box<dyn Scheduler> {
    config.build()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::SystemTime;

    use dorfq_forge_github::api::{
        CheckResult, Commit, Compare, IssueDetail, Merge, MergeState, OpenPr, PrDetail, RefEntry,
        RefUpdate, RulesetId, RunId, WorkflowRun,
    };

    use super::*;
    use crate::config::RefreshConfig;

    #[test]
    fn missing_scenario_names_the_path() {
        let config = ForgeConfig::Mock {
            scenario: PathBuf::from("no/such/scenario.toml"),
        };
        let error = forge(&config).err().unwrap();
        assert!(matches!(error, ProviderError::ReadScenario { .. }));
        assert!(
            error.to_string().contains("no/such/scenario.toml"),
            "{error}"
        );
    }

    #[test]
    fn github_with_unset_token_variable_names_it_before_any_request() {
        let var = "DORFQ_TEST_TOKEN_THAT_IS_NEVER_SET";
        assert!(env::var_os(var).is_none(), "{var} is set");
        // An address nothing listens on: a request would fail as a GitHub
        // error, not as a missing token.
        let config = ForgeConfig::GitHub {
            api_base: "https://127.0.0.1:1".to_owned(),
            owner: "alice".to_owned(),
            repo: "widgets".to_owned(),
            trunk: "main".to_owned(),
            label: "queue".to_owned(),
            checks: vec!["check".to_owned()],
            batch_checks: None,
            lock_heads: false,
            ci: None,
            refresh: RefreshConfig {
                listing_secs: 60,
                pr_secs: 120,
                batch_secs: 30,
            },
            webhook: None,
            status_issue: None,
            auth: AuthConfig::Token {
                env: var.to_owned(),
            },
        };
        let error = forge(&config).err().unwrap();
        assert!(
            matches!(error, ProviderError::MissingToken { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains(var), "{error}");
    }

    #[test]
    fn github_with_unset_webhook_secret_variable_names_it_before_any_request() {
        let var = "DORFQ_TEST_WEBHOOK_SECRET_THAT_IS_NEVER_SET";
        assert!(env::var_os(var).is_none(), "{var} is set");
        let mut config = github_app(test_tmpdir().join("no-such-app-key.pem"));
        let ForgeConfig::GitHub { webhook, .. } = &mut config else {
            unreachable!("github_app builds a GitHub configuration");
        };
        *webhook = Some(WebhookConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            secret_env: var.to_owned(),
        });
        let error = forge(&config).err().unwrap();
        assert!(
            matches!(error, ProviderError::MissingWebhookSecret { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains(var), "{error}");
    }

    #[test]
    fn the_webhook_receiver_binds_and_a_taken_address_is_named() {
        let settings = github_app(PathBuf::from("unused.pem"))
            .github_settings()
            .unwrap();
        let (listener, _hints) = listen(
            "127.0.0.1:0".parse().unwrap(),
            "secret".to_owned(),
            "alice/widgets".to_owned(),
            &settings,
        )
        .unwrap();
        let address = listener.address();
        assert_ne!(address.port(), 0);
        let error = listen(
            address,
            "secret".to_owned(),
            "alice/widgets".to_owned(),
            &settings,
        )
        .err()
        .unwrap();
        assert!(matches!(error, ProviderError::Listen { .. }), "{error:?}");
        assert!(error.to_string().contains(&address.to_string()), "{error}");
    }

    /// A GitHub configuration under the app credential, against an address
    /// nothing listens on: a request would fail as a GitHub error, not as a
    /// key problem.
    fn github_app(private_key_file: PathBuf) -> ForgeConfig {
        ForgeConfig::GitHub {
            api_base: "https://127.0.0.1:1".to_owned(),
            owner: "alice".to_owned(),
            repo: "widgets".to_owned(),
            trunk: "main".to_owned(),
            label: "queue".to_owned(),
            checks: vec!["check".to_owned()],
            batch_checks: None,
            lock_heads: false,
            ci: None,
            refresh: RefreshConfig {
                listing_secs: 60,
                pr_secs: 120,
                batch_secs: 30,
            },
            webhook: None,
            status_issue: None,
            auth: AuthConfig::App {
                client_id: "Iv23liTEST".to_owned(),
                private_key_file,
            },
        }
    }

    #[test]
    fn github_app_with_missing_key_file_names_it_before_any_request() {
        let path = test_tmpdir().join("no-such-app-key.pem");
        let error = forge(&github_app(path.clone())).err().unwrap();
        assert!(
            matches!(error, ProviderError::ReadPrivateKey { .. }),
            "{error:?}"
        );
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn github_app_with_a_file_that_is_not_a_key_names_it_and_why() {
        let path = test_tmpdir().join("not-a-key.pem");
        fs::write(&path, "just some text\n").unwrap();
        let error = forge(&github_app(path.clone())).err().unwrap();
        assert!(
            matches!(error, ProviderError::PrivateKey { .. }),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("not PEM"), "{message}");
    }

    #[test]
    fn github_app_with_a_key_file_builds_the_app_credential() {
        let path = test_tmpdir().join("app-key.pem");
        fs::write(
            &path,
            include_str!("../../dorfq-forge-github/testdata/app-key.pkcs1.pem"),
        )
        .unwrap();
        let auth = github_auth(&AuthConfig::App {
            client_id: "Iv23liTEST".to_owned(),
            private_key_file: path,
        });
        assert!(matches!(auth, Ok(Auth::App(_))));
    }

    fn test_tmpdir() -> PathBuf {
        PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
    }

    #[test]
    fn mock_has_scheduled_events_until_its_last_arrival() {
        let scenario = test_tmpdir().join("late-arrival-scenario.toml");
        fs::write(
            &scenario,
            "ci_ticks = 1\n[[pr]]\nnumber = 1\narrives_at = 1\n",
        )
        .unwrap();
        let mut built = forge(&ForgeConfig::Mock { scenario }).unwrap();
        assert!(built.status.is_none());
        assert!(built.gauges.is_none());
        let head = Sha::parse(&"b".repeat(40)).unwrap();
        let listing = (built.capture)(PrNumber(7), &head).unwrap().unwrap();
        assert_eq!(
            listing,
            HeadFiles {
                base: Sha::parse(&"0".repeat(40)).unwrap(),
                additions: 1,
                deletions: 0,
                changed_files: 1,
                files: vec![ChangedFile {
                    path: "pr/7".to_owned(),
                    additions: 1,
                    deletions: 0,
                    status: "modified".to_owned(),
                    previous: None,
                }],
            }
        );
        assert!((built.has_scheduled_events)());
        built.forge.observe().unwrap();
        assert!((built.has_scheduled_events)());
        built.forge.observe().unwrap();
        assert!(!(built.has_scheduled_events)());
    }

    /// A repository whose trunk and label exist and that answers nothing
    /// else but, when it holds one, one pull request's detail and files:
    /// enough for the forge to start and for a capture read.
    struct StartableRepo {
        remaining: Option<u64>,
        pr: Option<(PrDetail, Vec<ChangedFile>)>,
    }

    const NOT_AT_STARTUP: &str = "the forge asks only for the trunk and the label at startup";

    impl GitHubApi for StartableRepo {
        fn get_ref(&mut self, _: &str) -> Result<Option<Sha>, ApiError> {
            Ok(Some(Sha::parse(&"a".repeat(40)).unwrap()))
        }
        fn list_refs(&mut self, _: &str) -> Result<Vec<RefEntry>, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn create_ref(&mut self, _: &str, _: &Sha) -> Result<(), ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn update_ref(&mut self, _: &str, _: &Sha, _: bool) -> Result<RefUpdate, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn delete_ref(&mut self, _: &str) -> Result<(), ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn get_commit(&mut self, _: &Sha) -> Result<Commit, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn create_commit(&mut self, _: &str, _: &Sha, _: &[Sha]) -> Result<Sha, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn merge(&mut self, _: &str, _: &Sha, _: &str) -> Result<Merge, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn is_ancestor(&mut self, _: &Sha, _: &Sha) -> Result<bool, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn compare(&mut self, _: &Sha, _: &Sha) -> Result<Compare, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn list_labelled(&mut self, _: &str) -> Result<Vec<PrNumber>, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn open_pr_with_head(&mut self, _: &str) -> Result<Option<OpenPr>, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn get_pr(&mut self, _: PrNumber) -> Result<PrDetail, ApiError> {
            Ok(self.pr.clone().expect(NOT_AT_STARTUP).0)
        }
        fn pr_files(&mut self, _: PrNumber) -> Result<Vec<ChangedFile>, ApiError> {
            Ok(self.pr.clone().expect(NOT_AT_STARTUP).1)
        }
        fn set_base(&mut self, _: PrNumber, _: &str) -> Result<(), ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn merged_pr_heads(&mut self, _: &str, _: &str) -> Result<Vec<Sha>, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn checks(&mut self, _: &Sha) -> Result<Vec<CheckResult>, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn remove_label(&mut self, _: PrNumber, _: &str) -> Result<(), ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn list_comments(&mut self, _: PrNumber) -> Result<Vec<String>, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn create_comment(&mut self, _: PrNumber, _: &str) -> Result<(), ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn get_issue(&mut self, _: u64) -> Result<Option<IssueDetail>, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn set_issue_body(&mut self, _: u64, _: &str) -> Result<(), ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn label_exists(&mut self, _: &str) -> Result<bool, ApiError> {
            Ok(true)
        }
        fn find_ruleset(
            &mut self,
            _: &str,
        ) -> Result<Option<(RulesetId, BTreeSet<String>)>, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn create_ruleset(&mut self, _: &str, _: &BTreeSet<String>) -> Result<RulesetId, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn update_ruleset(&mut self, _: RulesetId, _: &BTreeSet<String>) -> Result<bool, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn delete_ruleset(&mut self, _: RulesetId) -> Result<(), ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn list_runs(&mut self, _: &Sha) -> Result<Vec<WorkflowRun>, ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn cancel_run(&mut self, _: RunId) -> Result<(), ApiError> {
            unreachable!("{NOT_AT_STARTUP}")
        }
        fn rate_limit_pause(&mut self) -> Option<SystemTime> {
            None
        }
        fn rate_limit_remaining(&self) -> Option<u64> {
            self.remaining
        }
    }

    #[test]
    fn github_gauges_read_the_forge() {
        let settings = github_app(PathBuf::from("unused.pem"))
            .github_settings()
            .unwrap();
        let api = StartableRepo {
            remaining: Some(4321),
            pr: None,
        };
        let built = github(Box::new(api), settings, None).unwrap();
        assert!(built.status.is_none());
        let mut gauges = built.gauges.expect("the GitHub forge has gauges");
        assert_eq!(
            gauges(),
            ForgeGauges {
                blocked_prs: 0,
                rate_limit_remaining: Some(4321),
            }
        );
    }

    #[test]
    fn github_capture_reads_the_forge() {
        let settings = github_app(PathBuf::from("unused.pem"))
            .github_settings()
            .unwrap();
        let head = Sha::parse(&"c".repeat(40)).unwrap();
        let base = Sha::parse(&"d".repeat(40)).unwrap();
        let files = vec![ChangedFile {
            path: "docs/new.md".to_owned(),
            additions: 4,
            deletions: 2,
            status: "renamed".to_owned(),
            previous: Some("docs/old.md".to_owned()),
        }];
        let detail = PrDetail {
            open: true,
            base: "main".to_owned(),
            head: head.clone(),
            head_branch: Some("topic".to_owned()),
            title: "Rename a document".to_owned(),
            labels: Vec::new(),
            merge_state: MergeState::Clean,
            draft: false,
            merged: false,
            base_sha: base.clone(),
            additions: Some(4),
            deletions: Some(2),
            changed_files: None,
        };
        let api = StartableRepo {
            remaining: None,
            pr: Some((
                PrDetail {
                    changed_files: Some(1),
                    ..detail.clone()
                },
                files.clone(),
            )),
        };
        let mut built = github(Box::new(api), settings.clone(), None).unwrap();
        assert_eq!(
            (built.capture)(PrNumber(9), &head).unwrap(),
            Ok(HeadFiles {
                base,
                additions: 4,
                deletions: 2,
                changed_files: 1,
                files: files.clone(),
            })
        );
        let moved = Sha::parse(&"e".repeat(40)).unwrap();
        assert_eq!(
            (built.capture)(PrNumber(9), &moved).unwrap(),
            Err(head.clone())
        );

        let api = StartableRepo {
            remaining: None,
            pr: Some((detail, files)),
        };
        let mut built = github(Box::new(api), settings, None).unwrap();
        let error = (built.capture)(PrNumber(9), &head).map(|_| ()).unwrap_err();
        let ForgeError::Permanent(error) = error else {
            panic!("a response without a count is permanent: {error:?}");
        };
        assert_eq!(error.to_string(), "GET /pulls/9: no changed_files field");
    }

    #[test]
    fn invalid_scenario_names_the_path_and_the_field() {
        let scenario = test_tmpdir().join("invalid-scenario.toml");
        fs::write(
            &scenario,
            "ci_ticks = 1\n[[pr]]\nnumber = 1\nsurprise = true\n",
        )
        .unwrap();
        let config = ForgeConfig::Mock {
            scenario: scenario.clone(),
        };
        let error = forge(&config).err().unwrap();
        assert!(matches!(error, ProviderError::Scenario { .. }), "{error:?}");
        let message = error.to_string();
        assert!(
            message.contains(&scenario.display().to_string()),
            "{message}"
        );
        assert!(message.contains("surprise"), "{message}");
    }
}
