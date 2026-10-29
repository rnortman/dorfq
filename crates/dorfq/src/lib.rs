//! DorfQ, a merge queue that pools pull requests into batched CI runs.
//!
//! This crate is the application: it reads a configuration, builds the forge
//! and the scheduler it names, and runs the driver loop that joins them
//! across the `dorfq_core` contract.

pub mod config;
pub mod driver;
pub mod journal;
pub mod providers;
pub mod telemetry;

#[cfg(test)]
mod e2e_tests;

/// The version of this crate, as the build stamped it.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The `User-Agent` this crate's HTTP clients send: `dorfq/<version>`.
pub fn user_agent() -> String {
    format!("dorfq/{}", version())
}

/// The host clock, Unix seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the host clock is after 1970")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_three_dot_separated_integers() {
        let v = version();
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(parts.len(), 3, "version {v:?} is not major.minor.patch");
        for part in parts {
            assert!(
                part.parse::<u64>().is_ok(),
                "version {v:?} has a non-integer component {part:?}"
            );
        }
    }
}
