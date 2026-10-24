//! The configuration file: which forge, which scheduler, and how the driver
//! paces its ticks.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use dorfq_forge_github::Settings;
use dorfq_scheduler::SchedulerConfig;
use serde::Deserialize;

/// A loaded configuration, with every path resolved and the scheduler's
/// parameters validated.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub forge: ForgeConfig,
    pub scheduler: SchedulerConfig,
    pub driver: DriverConfig,
}

/// The forge to build.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ForgeConfig {
    /// The in-memory mock, driven by a scenario file.
    Mock {
        /// The scenario file; relative paths are resolved against the
        /// configuration file's directory at load.
        scenario: PathBuf,
    },
    /// A GitHub repository, through the REST API.
    #[serde(rename = "github")]
    GitHub {
        /// `https://api.github.com`, or `https://<host>/api/v3` for GitHub
        /// Enterprise.
        api_base: String,
        owner: String,
        repo: String,
        /// The branch the queue lands on.
        trunk: String,
        /// The intent label.
        label: String,
        /// The check names that must pass, on pull request heads and batch
        /// tips.
        checks: Vec<String>,
        /// The credential the forge runs under.
        auth: AuthConfig,
    },
}

/// The credential the GitHub forge runs under.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AuthConfig {
    /// A fixed token read from the environment.
    Token {
        /// The environment variable holding the token; never the token.
        env: String,
    },
    /// A GitHub App.
    App {
        /// The app's Client ID (the numeric App ID also works).
        client_id: String,
        /// The app's private key, PEM; relative paths are resolved against
        /// the configuration file's directory at load.
        private_key_file: PathBuf,
    },
}

/// How the driver loop runs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverConfig {
    /// Milliseconds to sleep between ticks.
    pub tick_millis: u64,
}

/// A configuration that could not be loaded.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Read {
        path: PathBuf,
        error: std::io::Error,
    },
    /// The file is not a valid configuration; the message names the field.
    Parse { path: PathBuf, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, error } => {
                write!(f, "cannot read configuration {}: {error}", path.display())
            }
            ConfigError::Parse { path, message } => {
                write!(f, "invalid configuration {}: {message}", path.display())
            }
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ConfigError::Read { error, .. } => Some(error),
            ConfigError::Parse { .. } => None,
        }
    }
}

impl Config {
    /// Reads and parses the configuration file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = fs::read_to_string(path).map_err(|error| ConfigError::Read {
            path: path.to_owned(),
            error,
        })?;
        let dir = path.parent().unwrap_or(Path::new(""));
        Config::parse(&text, dir).map_err(|message| ConfigError::Parse {
            path: path.to_owned(),
            message,
        })
    }

    /// Parses configuration text, validates the scheduler's parameters, and
    /// resolves relative paths against `dir`.
    fn parse(text: &str, dir: &Path) -> Result<Config, String> {
        let mut config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config
            .scheduler
            .validate()
            .map_err(|e| format!("scheduler: {e}"))?;
        match &mut config.forge {
            ForgeConfig::Mock { scenario } => {
                let resolved = dir.join(scenario.as_path());
                *scenario = resolved;
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
                if !api_base.starts_with("https://") {
                    return Err(format!(
                        "forge.api_base {api_base:?} is not an https:// URL"
                    ));
                }
                let credential = match auth {
                    AuthConfig::Token { env } => ("auth.env", env.as_str()),
                    AuthConfig::App {
                        client_id,
                        private_key_file,
                    } => {
                        let resolved = dir.join(private_key_file.as_path());
                        *private_key_file = resolved;
                        ("auth.client_id", client_id.as_str())
                    }
                };
                for (field, value) in [
                    ("owner", owner.as_str()),
                    ("repo", repo.as_str()),
                    credential,
                ] {
                    if value.is_empty() {
                        return Err(format!("forge.{field} is empty"));
                    }
                }
                Settings {
                    trunk: trunk.clone(),
                    label: label.clone(),
                    checks: checks.clone(),
                }
                .validate()
                .map_err(|problem| format!("forge.{problem}"))?;
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[forge]
kind = "mock"
scenario = "scenario.toml"

[scheduler]
kind = "all-at-once"

[driver]
tick_millis = 0
"#;

    #[test]
    fn valid_configuration_parses() {
        let config = Config::parse(VALID, Path::new("conf")).unwrap();
        assert_eq!(
            config,
            Config {
                forge: ForgeConfig::Mock {
                    scenario: PathBuf::from("conf/scenario.toml"),
                },
                scheduler: SchedulerConfig::AllAtOnce {},
                driver: DriverConfig { tick_millis: 0 },
            }
        );
    }

    #[test]
    fn absolute_scenario_path_is_kept() {
        let text = VALID.replace("\"scenario.toml\"", "\"/abs/scenario.toml\"");
        let config = Config::parse(&text, Path::new("conf")).unwrap();
        assert_eq!(
            config.forge,
            ForgeConfig::Mock {
                scenario: PathBuf::from("/abs/scenario.toml"),
            }
        );
    }

    fn rejected(text: &str) -> String {
        Config::parse(text, Path::new("")).unwrap_err()
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let message = rejected(&format!("{VALID}\nextra = 1\n"));
        assert!(message.contains("extra"), "{message}");
    }

    #[test]
    fn unknown_field_in_a_provider_is_rejected() {
        let text = VALID.replace(
            "kind = \"all-at-once\"",
            "kind = \"all-at-once\"\nspeed = 3",
        );
        let message = rejected(&text);
        assert!(message.contains("speed"), "{message}");
        let text = VALID.replace("tick_millis = 0", "tick_millis = 0\nburst = 2");
        let message = rejected(&text);
        assert!(message.contains("burst"), "{message}");
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let message = rejected(&VALID.replace("\"mock\"", "\"gitlab\""));
        assert!(message.contains("gitlab"), "{message}");
        let message = rejected(&VALID.replace("\"all-at-once\"", "\"clever\""));
        assert!(message.contains("clever"), "{message}");
    }

    #[test]
    fn missing_field_is_rejected() {
        let message = rejected(&VALID.replace("scenario = \"scenario.toml\"", ""));
        assert!(message.contains("scenario"), "{message}");
        let message = rejected(&VALID.replace("tick_millis = 0", ""));
        assert!(message.contains("tick_millis"), "{message}");
        let message = rejected(&VALID.replace("kind = \"mock\"", ""));
        assert!(message.contains("kind"), "{message}");
        let message = rejected(&VALID.replace("[driver]\ntick_millis = 0", ""));
        assert!(message.contains("driver"), "{message}");
    }

    const GITHUB: &str = r#"
[forge]
kind = "github"
api_base = "https://api.github.com"
owner = "alice"
repo = "dorfq"
trunk = "main"
label = "queue"
checks = ["check"]

[forge.auth]
kind = "token"
env = "DORFQ_GITHUB_TOKEN"

[scheduler]
kind = "all-at-once"

[driver]
tick_millis = 60000
"#;

    #[test]
    fn github_configuration_parses() {
        let config = Config::parse(GITHUB, Path::new("conf")).unwrap();
        assert_eq!(
            config.forge,
            ForgeConfig::GitHub {
                api_base: "https://api.github.com".to_owned(),
                owner: "alice".to_owned(),
                repo: "dorfq".to_owned(),
                trunk: "main".to_owned(),
                label: "queue".to_owned(),
                checks: vec!["check".to_owned()],
                auth: AuthConfig::Token {
                    env: "DORFQ_GITHUB_TOKEN".to_owned(),
                },
            }
        );
        assert_eq!(config.driver, DriverConfig { tick_millis: 60000 });
    }

    const TOKEN_AUTH: &str = "kind = \"token\"\nenv = \"DORFQ_GITHUB_TOKEN\"\n";

    /// [`GITHUB`] with the `[forge.auth]` body replaced by `auth`.
    fn github_with_auth(auth: &str) -> String {
        assert!(GITHUB.contains(TOKEN_AUTH));
        GITHUB.replace(TOKEN_AUTH, auth)
    }

    fn auth_of(config: Config) -> AuthConfig {
        match config.forge {
            ForgeConfig::GitHub { auth, .. } => auth,
            other => panic!("not a github forge: {other:?}"),
        }
    }

    #[test]
    fn github_app_auth_parses_and_resolves_a_relative_key_path() {
        let text = github_with_auth(
            "kind = \"app\"\nclient_id = \"Iv23liABC\"\nprivate_key_file = \"dorfq-app.pem\"\n",
        );
        let config = Config::parse(&text, Path::new("conf")).unwrap();
        assert_eq!(
            auth_of(config),
            AuthConfig::App {
                client_id: "Iv23liABC".to_owned(),
                private_key_file: PathBuf::from("conf/dorfq-app.pem"),
            }
        );
    }

    #[test]
    fn github_app_absolute_key_path_is_kept() {
        let text = github_with_auth(
            "kind = \"app\"\nclient_id = \"Iv23liABC\"\nprivate_key_file = \"/run/key.pem\"\n",
        );
        let config = Config::parse(&text, Path::new("conf")).unwrap();
        assert_eq!(
            auth_of(config),
            AuthConfig::App {
                client_id: "Iv23liABC".to_owned(),
                private_key_file: PathBuf::from("/run/key.pem"),
            }
        );
    }

    #[test]
    fn github_missing_auth_is_rejected_naming_it() {
        let text = github_with_auth("").replace("[forge.auth]\n", "");
        let message = rejected(&text);
        assert!(message.contains("auth"), "{message}");
    }

    #[test]
    fn github_auth_unknown_kind_is_rejected_naming_it() {
        let message = rejected(&github_with_auth("kind = \"oauth\"\n"));
        assert!(message.contains("oauth"), "{message}");
    }

    #[test]
    fn github_auth_unknown_field_is_rejected_naming_it() {
        let message = rejected(&github_with_auth(&format!("{TOKEN_AUTH}scope = \"all\"\n")));
        assert!(message.contains("scope"), "{message}");
        // The token form's field is not accepted on the app form.
        let message = rejected(&github_with_auth(
            "kind = \"app\"\nclient_id = \"Iv23liABC\"\nprivate_key_file = \"k.pem\"\nenv = \"X\"\n",
        ));
        assert!(message.contains("env"), "{message}");
    }

    #[test]
    fn github_auth_missing_field_is_rejected_naming_it() {
        let message = rejected(&github_with_auth("kind = \"token\"\n"));
        assert!(message.contains("env"), "{message}");
        let message = rejected(&github_with_auth(
            "kind = \"app\"\nclient_id = \"Iv23liABC\"\n",
        ));
        assert!(message.contains("private_key_file"), "{message}");
        let message = rejected(&github_with_auth(
            "kind = \"app\"\nprivate_key_file = \"k.pem\"\n",
        ));
        assert!(message.contains("client_id"), "{message}");
    }

    #[test]
    fn github_auth_empty_field_is_rejected_naming_it() {
        let message = rejected(&github_with_auth("kind = \"token\"\nenv = \"\"\n"));
        assert_eq!(message, "forge.auth.env is empty");
        let message = rejected(&github_with_auth(
            "kind = \"app\"\nclient_id = \"\"\nprivate_key_file = \"k.pem\"\n",
        ));
        assert_eq!(message, "forge.auth.client_id is empty");
    }

    #[test]
    fn shipped_github_example_parses() {
        let text = include_str!("../examples/github.toml");
        let config = Config::parse(text, Path::new("")).unwrap();
        assert!(matches!(
            config.forge,
            ForgeConfig::GitHub {
                auth: AuthConfig::App { .. },
                ..
            }
        ));
    }

    #[test]
    fn github_missing_field_is_rejected_naming_it() {
        for field in ["api_base", "owner", "repo", "trunk", "label", "checks"] {
            let text: String = GITHUB
                .lines()
                .filter(|line| !line.starts_with(&format!("{field} =")))
                .map(|line| format!("{line}\n"))
                .collect();
            let message = rejected(&text);
            assert!(message.contains(field), "{field}: {message}");
        }
    }

    #[test]
    fn github_empty_field_is_rejected_naming_it() {
        for (field, value) in [
            ("owner", "\"alice\""),
            ("repo", "\"dorfq\""),
            ("trunk", "\"main\""),
            ("label", "\"queue\""),
        ] {
            let text = GITHUB.replace(&format!("{field} = {value}"), &format!("{field} = \"\""));
            let message = rejected(&text);
            assert_eq!(message, format!("forge.{field} is empty"));
        }
    }

    #[test]
    fn github_plain_http_base_is_rejected() {
        let text = GITHUB.replace("https://api.github.com", "http://api.github.com");
        let message = rejected(&text);
        assert!(message.contains("forge.api_base"), "{message}");
        assert!(message.contains("http://api.github.com"), "{message}");
    }

    #[test]
    fn github_empty_checks_are_rejected() {
        let message = rejected(&GITHUB.replace("[\"check\"]", "[]"));
        assert_eq!(message, "forge.checks names no check");
        let message = rejected(&GITHUB.replace("[\"check\"]", "[\"check\", \"\"]"));
        assert_eq!(message, "forge.checks[1] is empty");
    }

    #[test]
    fn github_token_env_of_the_old_form_is_rejected_naming_it() {
        let old = GITHUB.replace(
            "\n[forge.auth]\nkind = \"token\"\nenv = \"DORFQ_GITHUB_TOKEN\"\n",
            "token_env = \"DORFQ_GITHUB_TOKEN\"\n",
        );
        assert_ne!(old, GITHUB);
        let message = rejected(&old);
        assert!(message.contains("token_env"), "{message}");
    }

    #[test]
    fn github_unknown_field_is_rejected() {
        let text = GITHUB.replace("label = \"queue\"", "label = \"queue\"\ntoken = \"x\"");
        let message = rejected(&text);
        assert!(message.contains("token"), "{message}");
    }

    #[test]
    fn out_of_range_scheduler_parameter_is_rejected_by_name() {
        let text = VALID.replace(
            "kind = \"all-at-once\"",
            "kind = \"bisect\"\nlanes = 0\nmax_batch = 4",
        );
        let message = rejected(&text);
        assert!(message.contains("lanes"), "{message}");
    }

    #[test]
    fn unreadable_file_names_the_path() {
        let error = Config::load(Path::new("no/such/dorfq.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Read { .. }));
        assert!(error.to_string().contains("no/such/dorfq.toml"), "{error}");
    }
}
