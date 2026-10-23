//! The configuration file: which forge, which scheduler, and how the driver
//! paces its ticks.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A loaded configuration, with every path resolved.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
}

/// The scheduler to build.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SchedulerConfig {
    /// The stub that batches every ready pull request at once.
    AllAtOnce {},
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

    /// Parses configuration text, resolving relative paths against `dir`.
    fn parse(text: &str, dir: &Path) -> Result<Config, String> {
        let mut config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        match &mut config.forge {
            ForgeConfig::Mock { scenario } => {
                let resolved = dir.join(scenario.as_path());
                *scenario = resolved;
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

    #[test]
    fn unreadable_file_names_the_path() {
        let error = Config::load(Path::new("no/such/dorfq.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Read { .. }));
        assert!(error.to_string().contains("no/such/dorfq.toml"), "{error}");
    }
}
