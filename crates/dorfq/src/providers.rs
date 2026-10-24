//! Builds the forge and the scheduler a configuration names.

use std::cell::RefCell;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use dorfq_core::{
    BatchId, BatchStart, EjectReason, Forge, ForgeError, Observation, PrNumber, Scheduler,
};
use dorfq_forge_github::api::{ApiError, Auth, RestClient};
use dorfq_forge_github::app::AppKey;
use dorfq_forge_github::{GitHubForge, Settings, system_clock};
use dorfq_forge_mock::{MockForge, Scenario, ScenarioError};
use dorfq_scheduler::SchedulerConfig;

use crate::config::{AuthConfig, ForgeConfig};

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
}

/// A [`MockForge`] behind a [`Forge`], still reachable through the other
/// handles on it.
pub(crate) struct SharedMock(pub(crate) Rc<RefCell<MockForge>>);

impl Forge for SharedMock {
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
}

/// Builds the configured forge. Either forge writes its messages to stdout.
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
            let forge = SharedMock(Rc::clone(&mock));
            Ok(BuiltForge {
                forge: Box::new(forge),
                has_scheduled_events: Box::new(move || mock.borrow().awaiting_arrivals()),
            })
        }
        ForgeConfig::GitHub {
            api_base,
            owner,
            repo,
            trunk,
            label,
            checks,
            auth,
        } => {
            let auth = github_auth(auth)?;
            let user_agent = format!("dorfq/{}", crate::version());
            let client = RestClient::new(api_base, owner, repo, auth, &user_agent);
            let settings = Settings {
                trunk: trunk.clone(),
                label: label.clone(),
                checks: checks.clone(),
            };
            let forge = GitHubForge::new(
                Box::new(client),
                settings,
                Box::new(system_clock),
                Box::new(io::stdout()),
            )
            .map_err(ProviderError::GitHub)?;
            Ok(BuiltForge {
                forge: Box::new(forge),
                has_scheduled_events: Box::new(|| false),
            })
        }
    }
}

/// Loads the credential the configuration names: the token from its
/// environment variable, or the app's key from its file.
fn github_auth(config: &AuthConfig) -> Result<Auth, ProviderError> {
    match config {
        AuthConfig::Token { env: var } => match env::var(var) {
            Ok(token) if !token.is_empty() => Ok(Auth::Token(token)),
            _ => Err(ProviderError::MissingToken { var: var.clone() }),
        },
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
    use super::*;

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
        assert!((built.has_scheduled_events)());
        built.forge.observe().unwrap();
        assert!((built.has_scheduled_events)());
        built.forge.observe().unwrap();
        assert!(!(built.has_scheduled_events)());
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
