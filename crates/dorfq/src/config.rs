//! The configuration file: which forge, which scheduler, where the journal
//! goes, and where telemetry goes.

use std::error::Error;
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dorfq_forge_github::refresh::Refresh;
use dorfq_forge_github::{Ci, Settings};
use dorfq_scheduler::SchedulerConfig;
use serde::Deserialize;
use ureq::http::Uri;

/// A loaded configuration, with every path resolved and the scheduler's
/// parameters validated.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The tick journal's path; absent, no journal is written. Relative
    /// paths are resolved against the configuration file's directory at
    /// load.
    #[serde(default)]
    pub journal: Option<PathBuf>,
    /// The events file's path, beside the journal and rotated with it;
    /// absent, no events file is written. Needs `journal`. Relative paths
    /// are resolved as `journal`'s.
    #[serde(default)]
    pub events: Option<PathBuf>,
    /// The capture file's path, beside the journal and rotated with it: each
    /// pull request head's changed files, read from the forge at first
    /// sight; absent, nothing is captured. Needs `journal`. Relative paths
    /// are resolved as `journal`'s.
    #[serde(default)]
    pub capture: Option<PathBuf>,
    /// Where the queue pushes its gauges and events; absent, nothing is
    /// pushed. Needs `journal`.
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
    pub forge: ForgeConfig,
    pub scheduler: SchedulerConfig,
}

/// Where the queue pushes its telemetry over OTLP/HTTP.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// The collector's OTLP/HTTP base URL, `http://` or `https://`;
    /// `/v1/metrics` and `/v1/logs` are appended. A trailing `/` is
    /// dropped at load.
    pub otlp_endpoint: String,
    /// Seconds between exports; 1 to [`MAX_INTERVAL_SECS`].
    pub interval_secs: u64,
    /// The environment variable holding the `Authorization` header's
    /// value, whole (`Basic …`, `Bearer …`); absent, no header is sent.
    /// Never the value.
    #[serde(default)]
    pub authorization_env: Option<String>,
}

/// The longest `interval_secs`, a day. The exporter's clock is an
/// `Instant`, which panics on an interval it cannot add.
pub const MAX_INTERVAL_SECS: u64 = 86_400;

impl TelemetryConfig {
    /// Checks the table and trims the endpoint's trailing `/`.
    fn validate(&mut self) -> Result<(), String> {
        let endpoint = &self.otlp_endpoint;
        // Credentials or a token may sit after an `@`, a `?` or a `#`, so
        // such an endpoint is never written into the message.
        let shown = if endpoint.contains(['@', '?', '#']) {
            String::new()
        } else {
            format!(" {endpoint:?}")
        };
        let problem = |reason: &str| format!("telemetry.otlp_endpoint{shown} {reason}");
        let uri: Uri = endpoint
            .parse()
            .map_err(|error| problem(&format!("is not a URL: {error}")))?;
        if !matches!(uri.scheme_str(), Some("http" | "https")) {
            return Err(problem("is not an http:// or https:// URL"));
        }
        let (Some(authority), Some(host)) = (uri.authority(), uri.host()) else {
            return Err(problem("has no host"));
        };
        if host.is_empty() {
            return Err(problem("has no host"));
        }
        if authority.as_str().contains('@') {
            return Err(problem("carries credentials; use authorization_env"));
        }
        // Without credentials the authority is the host and, when it is
        // longer, a `:` and the port.
        if authority.as_str().len() > host.len() && uri.port_u16().is_none() {
            return Err(problem("has an invalid port"));
        }
        // `Uri` keeps a query but discards a fragment, so both are read off
        // the text.
        if endpoint.contains(['?', '#']) {
            return Err(problem("has a query or fragment"));
        }
        let trimmed = endpoint.trim_end_matches('/').len();
        self.otlp_endpoint.truncate(trimmed);
        if self.interval_secs == 0 {
            return Err("telemetry.interval_secs is 0".to_owned());
        }
        if self.interval_secs > MAX_INTERVAL_SECS {
            return Err(format!(
                "telemetry.interval_secs is above {MAX_INTERVAL_SECS}"
            ));
        }
        if self.authorization_env.as_deref() == Some("") {
            return Err("telemetry.authorization_env is empty".to_owned());
        }
        Ok(())
    }
}

/// The forge to build.
#[expect(
    clippy::large_enum_variant,
    reason = "one value per process, read once at startup"
)]
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
        /// The check names that must pass on pull request heads.
        checks: Vec<String>,
        /// The check names that must pass on batch tips; absent, the same
        /// as `checks`.
        #[serde(default)]
        batch_checks: Option<Vec<String>>,
        /// Whether a batched pull request's head branch is locked by the
        /// queue's ruleset while it is batched; absent, off.
        #[serde(default)]
        lock_heads: bool,
        /// The CI system whose runs the queue cancels when it retires a
        /// batch that is still testing; `"actions"`, or absent to cancel
        /// nothing.
        #[serde(default)]
        ci: Option<Ci>,
        /// How often each entity the forge caches is read unprompted.
        refresh: RefreshConfig,
        /// The webhook receiver; absent, nothing listens and the refresh
        /// intervals are the only clock.
        #[serde(default)]
        webhook: Option<WebhookConfig>,
        /// The number of an issue whose body the queue keeps as its status;
        /// absent, no issue is edited. Needs `journal`.
        #[serde(default)]
        status_issue: Option<u64>,
        /// The credential the forge runs under.
        auth: AuthConfig,
    },
}

/// How often the GitHub forge reads each entity it caches unprompted, in
/// seconds; each at least 1. No defaults: the right values differ by an
/// order of magnitude between a queue receiving webhooks and one polling.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshConfig {
    /// The labelled pull requests and the batch records.
    pub listing_secs: u64,
    /// Each queued pull request.
    pub pr_secs: u64,
    /// Each testing batch's checks, and trunk.
    pub batch_secs: u64,
}

/// Where the GitHub forge receives webhook deliveries.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    /// The address the receiver binds: an IP address and a port.
    #[serde(deserialize_with = "socket_address")]
    pub listen: SocketAddr,
    /// The environment variable holding the webhook secret; never the
    /// secret.
    pub secret_env: String,
}

/// A [`SocketAddr`] from its text, the error naming `listen`.
fn socket_address<'de, D: serde::Deserializer<'de>>(d: D) -> Result<SocketAddr, D::Error> {
    let text = String::deserialize(d)?;
    text.parse().map_err(|e| {
        serde::de::Error::custom(format!(
            "forge.webhook.listen {text:?} is not an IP address and port: {e}"
        ))
    })
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
        if config.events.is_some() && config.journal.is_none() {
            return Err("events needs journal".to_owned());
        }
        if config.capture.is_some() && config.journal.is_none() {
            return Err("capture needs journal".to_owned());
        }
        if let ForgeConfig::GitHub {
            status_issue: Some(_),
            ..
        } = config.forge
            && config.journal.is_none()
        {
            return Err("forge.status_issue needs journal".to_owned());
        }
        if let Some(telemetry) = &mut config.telemetry {
            if config.journal.is_none() {
                return Err("telemetry needs journal".to_owned());
            }
            telemetry.validate()?;
        }
        for path in [&mut config.journal, &mut config.events, &mut config.capture]
            .into_iter()
            .flatten()
        {
            *path = dir.join(path.as_path());
        }
        match &mut config.forge {
            ForgeConfig::Mock { scenario } => {
                let resolved = dir.join(scenario.as_path());
                *scenario = resolved;
            }
            ForgeConfig::GitHub {
                api_base,
                owner,
                repo,
                auth,
                webhook,
                ..
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
                let secret_env = webhook
                    .as_ref()
                    .map(|w| ("webhook.secret_env", w.secret_env.as_str()));
                for (field, value) in [
                    ("owner", owner.as_str()),
                    ("repo", repo.as_str()),
                    credential,
                ]
                .into_iter()
                .chain(secret_env)
                {
                    if value.is_empty() {
                        return Err(format!("forge.{field} is empty"));
                    }
                }
            }
        }
        if let Some(settings) = config.forge.github_settings() {
            settings
                .validate()
                .map_err(|problem| format!("forge.{problem}"))?;
        }
        Ok(config)
    }
}

impl ForgeConfig {
    /// The forge's kind as the configuration names it.
    pub fn kind(&self) -> &'static str {
        match self {
            ForgeConfig::Mock { .. } => "mock",
            ForgeConfig::GitHub { .. } => "github",
        }
    }

    /// The GitHub forge's settings, `None` for any other forge:
    /// `batch_checks` absent is `checks`.
    pub fn github_settings(&self) -> Option<Settings> {
        let ForgeConfig::GitHub {
            trunk,
            label,
            checks,
            batch_checks,
            lock_heads,
            ci,
            refresh,
            status_issue,
            ..
        } = self
        else {
            return None;
        };
        Some(Settings {
            trunk: trunk.clone(),
            label: label.clone(),
            checks: checks.clone(),
            batch_checks: batch_checks.as_ref().unwrap_or(checks).clone(),
            lock_heads: *lock_heads,
            ci: *ci,
            refresh: Refresh {
                listing: Duration::from_secs(refresh.listing_secs),
                pr: Duration::from_secs(refresh.pr_secs),
                batch: Duration::from_secs(refresh.batch_secs),
            },
            status_issue: *status_issue,
        })
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
"#;

    #[test]
    fn valid_configuration_parses() {
        let config = Config::parse(VALID, Path::new("conf")).unwrap();
        assert_eq!(
            config,
            Config {
                journal: None,
                events: None,
                capture: None,
                telemetry: None,
                forge: ForgeConfig::Mock {
                    scenario: PathBuf::from("conf/scenario.toml"),
                },
                scheduler: SchedulerConfig::AllAtOnce {},
            }
        );
    }

    #[test]
    fn a_relative_journal_path_is_resolved_and_an_absolute_one_kept() {
        let journal = |path: &str| {
            Config::parse(&format!("journal = {path:?}\n{VALID}"), Path::new("conf"))
                .unwrap()
                .journal
        };
        assert_eq!(
            journal("dorfq.journal"),
            Some(PathBuf::from("conf/dorfq.journal"))
        );
        assert_eq!(
            journal("/var/lib/q.journal"),
            Some(PathBuf::from("/var/lib/q.journal"))
        );
    }

    #[test]
    fn a_relative_events_path_is_resolved_and_an_absolute_one_kept() {
        let events = |path: &str| {
            Config::parse(
                &format!("journal = \"q.journal\"\nevents = {path:?}\n{VALID}"),
                Path::new("conf"),
            )
            .unwrap()
            .events
        };
        assert_eq!(events("q.events"), Some(PathBuf::from("conf/q.events")));
        assert_eq!(
            events("/var/lib/q.events"),
            Some(PathBuf::from("/var/lib/q.events"))
        );
    }

    #[test]
    fn events_without_a_journal_is_rejected_naming_both() {
        let error = Config::parse(
            &format!("events = \"q.events\"\n{VALID}"),
            Path::new("conf"),
        )
        .unwrap_err();
        assert_eq!(error, "events needs journal");
    }

    #[test]
    fn a_relative_capture_path_is_resolved_and_an_absolute_one_kept() {
        let capture = |path: &str| {
            Config::parse(
                &format!("journal = \"q.journal\"\ncapture = {path:?}\n{VALID}"),
                Path::new("conf"),
            )
            .unwrap()
            .capture
        };
        assert_eq!(capture("q.capture"), Some(PathBuf::from("conf/q.capture")));
        assert_eq!(
            capture("/var/lib/q.capture"),
            Some(PathBuf::from("/var/lib/q.capture"))
        );
    }

    #[test]
    fn capture_without_a_journal_is_rejected_naming_both() {
        let error = Config::parse(
            &format!("capture = \"q.capture\"\n{VALID}"),
            Path::new("conf"),
        )
        .unwrap_err();
        assert_eq!(error, "capture needs journal");
    }

    /// [`VALID`] with a journal and a `[telemetry]` table holding `body`.
    fn with_telemetry(base: &str, body: &str) -> String {
        format!("journal = \"q.journal\"\n{base}\n[telemetry]\n{body}")
    }

    fn telemetry_of(text: &str) -> TelemetryConfig {
        Config::parse(text, Path::new("conf"))
            .unwrap()
            .telemetry
            .unwrap()
    }

    #[test]
    fn telemetry_parses_under_either_forge_and_is_absent_unless_set() {
        assert_eq!(Config::parse(VALID, Path::new("")).unwrap().telemetry, None);
        let body = "otlp_endpoint = \"http://127.0.0.1:4318\"\ninterval_secs = 60\n";
        let expected = TelemetryConfig {
            otlp_endpoint: "http://127.0.0.1:4318".to_owned(),
            interval_secs: 60,
            authorization_env: None,
        };
        assert_eq!(telemetry_of(&with_telemetry(VALID, body)), expected);
        assert_eq!(telemetry_of(&with_telemetry(GITHUB, body)), expected);
        let authorised = with_telemetry(
            GITHUB,
            &format!("{body}authorization_env = \"DORFQ_OTLP_AUTHORIZATION\"\n"),
        );
        assert_eq!(
            telemetry_of(&authorised).authorization_env.as_deref(),
            Some("DORFQ_OTLP_AUTHORIZATION")
        );
    }

    #[test]
    fn telemetry_endpoint_keeps_its_path_and_loses_a_trailing_slash() {
        for (given, kept) in [
            ("https://otlp.example/otlp/", "https://otlp.example/otlp"),
            ("http://collector:4318//", "http://collector:4318"),
            ("http://collector/", "http://collector"),
            ("http://[::1]:4318", "http://[::1]:4318"),
        ] {
            let text = with_telemetry(
                VALID,
                &format!("otlp_endpoint = {given:?}\ninterval_secs = 1\n"),
            );
            assert_eq!(telemetry_of(&text).otlp_endpoint, kept, "{given}");
        }
    }

    #[test]
    fn telemetry_without_a_journal_is_rejected_naming_both() {
        let text =
            format!("{VALID}\n[telemetry]\notlp_endpoint = \"http://host\"\ninterval_secs = 60\n");
        assert_eq!(rejected(&text), "telemetry needs journal");
    }

    #[test]
    fn telemetry_endpoint_that_is_no_usable_url_is_rejected_saying_why() {
        for (endpoint, reason) in [
            ("http://ho st", "is not a URL: "),
            ("ftp://host", "is not an http:// or https:// URL"),
            ("host:4318", "is not an http:// or https:// URL"),
            ("http://:4318", "has no host"),
            ("http://host:99999", "has an invalid port"),
            ("http://host:", "has an invalid port"),
        ] {
            let message = rejected(&with_telemetry(
                VALID,
                &format!("otlp_endpoint = {endpoint:?}\ninterval_secs = 60\n"),
            ));
            let prefix = format!("telemetry.otlp_endpoint {endpoint:?} {reason}");
            assert!(message.starts_with(&prefix), "{endpoint}: {message}");
        }
    }

    #[test]
    fn telemetry_endpoint_that_may_carry_a_secret_is_rejected_without_echoing_it() {
        for (endpoint, reason) in [
            (
                "http://u:secret@host",
                "carries credentials; use authorization_env",
            ),
            ("ftp://u:secret@host", "is not an http:// or https:// URL"),
            ("http://u:sec ret@host", "is not a URL: "),
            ("http://host/?api-key=secret", "has a query or fragment"),
            ("http://host/#secret", "has a query or fragment"),
        ] {
            let message = rejected(&with_telemetry(
                VALID,
                &format!("otlp_endpoint = {endpoint:?}\ninterval_secs = 60\n"),
            ));
            let prefix = format!("telemetry.otlp_endpoint {reason}");
            assert!(message.starts_with(&prefix), "{endpoint}: {message}");
            assert!(!message.contains("secret"), "{endpoint}: {message}");
        }
    }

    #[test]
    fn telemetry_interval_out_of_range_and_empty_variable_are_rejected() {
        let endpoint = "otlp_endpoint = \"http://host\"\n";
        assert_eq!(
            rejected(&with_telemetry(
                VALID,
                &format!("{endpoint}interval_secs = 0\n")
            )),
            "telemetry.interval_secs is 0"
        );
        for interval in [MAX_INTERVAL_SECS + 1, i64::MAX as u64] {
            assert_eq!(
                rejected(&with_telemetry(
                    VALID,
                    &format!("{endpoint}interval_secs = {interval}\n")
                )),
                "telemetry.interval_secs is above 86400"
            );
        }
        let at_most = format!("{endpoint}interval_secs = {MAX_INTERVAL_SECS}\n");
        let config = Config::parse(&with_telemetry(VALID, &at_most), Path::new("")).unwrap();
        assert_eq!(
            config.telemetry.map(|t| t.interval_secs),
            Some(MAX_INTERVAL_SECS)
        );
        assert_eq!(
            rejected(&with_telemetry(
                VALID,
                &format!("{endpoint}interval_secs = 1\nauthorization_env = \"\"\n")
            )),
            "telemetry.authorization_env is empty"
        );
    }

    #[test]
    fn telemetry_unknown_or_missing_field_is_rejected_naming_it() {
        let message = rejected(&with_telemetry(
            VALID,
            "otlp_endpoint = \"http://host\"\ninterval_sec = 60\n",
        ));
        assert!(message.contains("interval_sec"), "{message}");
        let message = rejected(&with_telemetry(VALID, "interval_secs = 60\n"));
        assert!(message.contains("otlp_endpoint"), "{message}");
    }

    #[test]
    fn the_forge_kind_is_the_configured_name() {
        let mock = Config::parse(VALID, Path::new("")).unwrap();
        assert_eq!(mock.forge.kind(), "mock");
        assert_eq!(
            Config::parse(GITHUB, Path::new("")).unwrap().forge.kind(),
            "github"
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
        let message = rejected(&VALID.replace("kind = \"mock\"", ""));
        assert!(message.contains("kind"), "{message}");
    }

    #[test]
    fn the_driver_table_is_rejected_naming_it() {
        let message = rejected(&format!("{VALID}\n[driver]\ntick_millis = 0\n"));
        assert!(message.contains("driver"), "{message}");
    }

    #[test]
    fn a_webhook_table_under_the_mock_is_rejected_naming_it() {
        let text = VALID.replace(
            "scenario = \"scenario.toml\"\n",
            "scenario = \"scenario.toml\"\n\n[forge.webhook]\nlisten = \"127.0.0.1:8787\"\nsecret_env = \"S\"\n",
        );
        let message = rejected(&text);
        assert!(message.contains("webhook"), "{message}");
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

[forge.refresh]
listing_secs = 60
pr_secs = 120
batch_secs = 30

[forge.auth]
kind = "token"
env = "DORFQ_GITHUB_TOKEN"

[scheduler]
kind = "all-at-once"
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
                    env: "DORFQ_GITHUB_TOKEN".to_owned(),
                },
            }
        );
    }

    /// [`GITHUB`] with `status_issue = {number}` under `[forge]`, and a
    /// journal when `journal` is set.
    fn github_with_status_issue(number: &str, journal: bool) -> String {
        let text = GITHUB.replace(
            "checks = [\"check\"]\n",
            &format!("checks = [\"check\"]\nstatus_issue = {number}\n"),
        );
        if journal {
            format!("journal = \"q.journal\"\n{text}")
        } else {
            text
        }
    }

    #[test]
    fn github_status_issue_is_absent_unless_set_and_passed_through() {
        let status_issue = |text: &str| {
            Config::parse(text, Path::new("conf"))
                .unwrap()
                .forge
                .github_settings()
                .unwrap()
                .status_issue
        };
        assert_eq!(status_issue(GITHUB), None);
        assert_eq!(
            status_issue(&github_with_status_issue("12", true)),
            Some(12)
        );
    }

    #[test]
    fn github_status_issue_without_a_journal_is_rejected_naming_both() {
        assert_eq!(
            rejected(&github_with_status_issue("12", false)),
            "forge.status_issue needs journal"
        );
    }

    #[test]
    fn github_status_issue_zero_is_rejected() {
        assert_eq!(
            rejected(&github_with_status_issue("0", true)),
            "forge.status_issue is 0"
        );
    }

    #[test]
    fn a_status_issue_under_the_mock_is_rejected_naming_it() {
        let text = VALID.replace(
            "scenario = \"scenario.toml\"\n",
            "scenario = \"scenario.toml\"\nstatus_issue = 12\n",
        );
        let message = rejected(&format!("journal = \"q.journal\"\n{text}"));
        assert!(message.contains("status_issue"), "{message}");
    }

    /// [`GITHUB`] with a `[forge.webhook]` table holding `body`.
    fn github_with_webhook(body: &str) -> String {
        GITHUB.replace(
            "\n[forge.auth]\n",
            &format!("\n[forge.webhook]\n{body}\n[forge.auth]\n"),
        )
    }

    fn webhook_of(config: Config) -> Option<WebhookConfig> {
        match config.forge {
            ForgeConfig::GitHub { webhook, .. } => webhook,
            other => panic!("not a github forge: {other:?}"),
        }
    }

    #[test]
    fn github_webhook_parses_its_address_and_variable() {
        let text = github_with_webhook(
            "listen = \"127.0.0.1:8787\"\nsecret_env = \"DORFQ_WEBHOOK_SECRET\"\n",
        );
        let config = Config::parse(&text, Path::new("conf")).unwrap();
        assert_eq!(
            webhook_of(config),
            Some(WebhookConfig {
                listen: "127.0.0.1:8787".parse().unwrap(),
                secret_env: "DORFQ_WEBHOOK_SECRET".to_owned(),
            })
        );
    }

    #[test]
    fn github_webhook_listen_that_is_not_a_socket_address_is_rejected() {
        for listen in ["localhost:8787", "127.0.0.1", "8787"] {
            let text = github_with_webhook(&format!(
                "listen = \"{listen}\"\nsecret_env = \"DORFQ_WEBHOOK_SECRET\"\n"
            ));
            let message = rejected(&text);
            assert!(message.contains("listen"), "{listen}: {message}");
        }
    }

    #[test]
    fn github_webhook_missing_or_empty_field_is_rejected_naming_it() {
        let message = rejected(&github_with_webhook("listen = \"127.0.0.1:8787\"\n"));
        assert!(message.contains("secret_env"), "{message}");
        let message = rejected(&github_with_webhook("secret_env = \"S\"\n"));
        assert!(message.contains("listen"), "{message}");
        let message = rejected(&github_with_webhook(
            "listen = \"127.0.0.1:8787\"\nsecret_env = \"\"\n",
        ));
        assert_eq!(message, "forge.webhook.secret_env is empty");
        let message = rejected(&github_with_webhook(
            "listen = \"127.0.0.1:8787\"\nsecret_env = \"S\"\nsecret = \"x\"\n",
        ));
        assert!(message.contains("secret"), "{message}");
    }

    #[test]
    fn github_batch_checks_are_passed_through_and_rejected_when_empty() {
        let text = GITHUB.replace(
            "checks = [\"check\"]\n",
            "checks = [\"check\"]\nbatch_checks = [\"merge\", \"slow\"]\n",
        );
        let settings = |text: &str| {
            Config::parse(text, Path::new("conf"))
                .unwrap()
                .forge
                .github_settings()
                .unwrap()
        };
        let given = settings(&text);
        assert_eq!(given.checks, ["check"]);
        assert_eq!(given.batch_checks, ["merge", "slow"]);
        assert_eq!(settings(GITHUB).batch_checks, ["check"]);
        let empty = GITHUB.replace(
            "checks = [\"check\"]\n",
            "checks = [\"check\"]\nbatch_checks = []\n",
        );
        assert_eq!(rejected(&empty), "forge.batch_checks names no check");
        let blank = GITHUB.replace(
            "checks = [\"check\"]\n",
            "checks = [\"check\"]\nbatch_checks = [\"\"]\n",
        );
        assert_eq!(rejected(&blank), "forge.batch_checks[0] is empty");
    }

    #[test]
    fn github_refresh_intervals_are_passed_through() {
        let settings = Config::parse(GITHUB, Path::new("conf"))
            .unwrap()
            .forge
            .github_settings()
            .unwrap();
        assert_eq!(
            settings.refresh,
            Refresh {
                listing: Duration::from_secs(60),
                pr: Duration::from_secs(120),
                batch: Duration::from_secs(30),
            }
        );
    }

    #[test]
    fn github_missing_refresh_or_interval_is_rejected_naming_it() {
        let message = rejected(&GITHUB.replace(
            "[forge.refresh]\nlisting_secs = 60\npr_secs = 120\nbatch_secs = 30\n",
            "",
        ));
        assert!(message.contains("refresh"), "{message}");
        for field in ["listing_secs", "pr_secs", "batch_secs"] {
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
    fn github_zero_interval_is_rejected_naming_it() {
        let message = rejected(&GITHUB.replace("pr_secs = 120", "pr_secs = 0"));
        assert!(
            message.starts_with("forge.refresh.pr_secs is 0ns"),
            "{message}"
        );
    }

    #[test]
    fn github_lock_heads_is_off_unless_set() {
        let lock_heads = |text: &str| {
            Config::parse(text, Path::new("conf"))
                .unwrap()
                .forge
                .github_settings()
                .unwrap()
                .lock_heads
        };
        assert!(!lock_heads(GITHUB));
        let on = GITHUB.replace(
            "checks = [\"check\"]\n",
            "checks = [\"check\"]\nlock_heads = true\n",
        );
        assert!(lock_heads(&on));
        let off = GITHUB.replace(
            "checks = [\"check\"]\n",
            "checks = [\"check\"]\nlock_heads = false\n",
        );
        assert!(!lock_heads(&off));
    }

    #[test]
    fn github_ci_is_absent_unless_set_and_names_a_known_system() {
        let with_ci = |value: &str| {
            GITHUB.replace(
                "checks = [\"check\"]\n",
                &format!("checks = [\"check\"]\nci = {value}\n"),
            )
        };
        let ci = |text: &str| {
            Config::parse(text, Path::new("conf"))
                .unwrap()
                .forge
                .github_settings()
                .unwrap()
                .ci
        };
        assert_eq!(ci(GITHUB), None);
        assert_eq!(ci(&with_ci("\"actions\"")), Some(Ci::Actions));
        let message = rejected(&with_ci("\"buildkite\""));
        assert!(message.contains("buildkite"), "{message}");
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
                webhook: Some(_),
                ..
            }
        ));
        assert!(matches!(config.scheduler, SchedulerConfig::Bayes(_)));
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
