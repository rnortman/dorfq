//! Builds the forge and the scheduler a configuration names.

use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use dorfq_core::{
    BatchId, BatchStart, EjectReason, Forge, ForgeError, Observation, PrNumber, Scheduler,
};
use dorfq_forge_mock::{MockForge, Scenario, ScenarioError};
use dorfq_scheduler::AllAtOnce;

use crate::config::{ForgeConfig, SchedulerConfig};

/// A provider that could not be built.
#[derive(Debug)]
pub enum ProviderError {
    /// The mock's scenario file could not be read.
    ReadScenario { path: PathBuf, error: io::Error },
    /// The mock's scenario file is invalid.
    Scenario { path: PathBuf, error: ScenarioError },
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
        }
    }
}

impl Error for ProviderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ProviderError::ReadScenario { error, .. } => Some(error),
            ProviderError::Scenario { error, .. } => Some(error),
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

/// Builds the configured forge. The mock writes its messages to stdout.
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
    }
}

/// Builds the configured scheduler.
pub fn scheduler(config: &SchedulerConfig) -> Box<dyn Scheduler> {
    match config {
        SchedulerConfig::AllAtOnce {} => Box::new(AllAtOnce::new()),
    }
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
