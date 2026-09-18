//! `config.toml` and its `HIVEMIND_*` environment overrides.
//!
//! Configuration is validated once at startup with messages that say what to
//! fix, not just what was wrong (SPEC §13.1). Every key has a working default,
//! so a missing file is a valid configuration rather than an error.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Default size limit for a single attachment (SPEC §6.3).
pub const DEFAULT_MAX_ATTACHMENT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Default threshold below which a blob ships with the message (SPEC §8).
pub const DEFAULT_INLINE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Default port for the peer listener (SPEC §3).
pub const DEFAULT_PEER_PORT: u16 = 8400;
/// Default port for the loopback listener (SPEC §3).
pub const DEFAULT_LOCAL_PORT: u16 = 8401;

/// Everything `config.toml` can say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// This node's display name, shown to peers.
    pub name: String,
    /// The human who owns this machine, used for fan-out addressing.
    pub owner: Option<String>,
    /// The peer listener's port.
    pub peer_port: u16,
    /// The loopback listener's port.
    pub local_port: u16,
    /// Post a desktop notification when mail arrives (SPEC §9.4).
    pub notifications: bool,
    /// Fetch non-inline attachments as soon as a message arrives, rather than
    /// on first access (SPEC §8).
    pub prefetch: bool,
    /// Largest attachment this node will accept.
    pub max_attachment_bytes: u64,
    /// Attachments at or below this size ship with the message.
    pub inline_max_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: default_name(),
            owner: None,
            peer_port: DEFAULT_PEER_PORT,
            local_port: DEFAULT_LOCAL_PORT,
            notifications: true,
            prefetch: false,
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            inline_max_bytes: DEFAULT_INLINE_MAX_BYTES,
        }
    }
}

/// The machine's hostname, or something honest if it will not say.
fn default_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "hivemind-node".to_owned())
}

/// Why a configuration was not acceptable.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("could not read {path}")]
    Io {
        /// The file in question.
        path: String,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The file was not valid TOML, or had a key we do not know.
    #[error("{path} is not a valid hivemind config: {detail}")]
    Malformed {
        /// The file in question.
        path: String,
        /// What the parser said.
        detail: String,
    },
    /// A value parsed but does not make sense.
    #[error("{key} is {value}, but {expectation}")]
    Invalid {
        /// Which setting.
        key: &'static str,
        /// What it was set to.
        value: String,
        /// What it should have been.
        expectation: &'static str,
    },
}

impl Config {
    /// Load `config.toml` from `dir`, apply `HIVEMIND_*` overrides, validate.
    ///
    /// A missing file is not an error: every key has a default.
    ///
    /// # Errors
    /// [`ConfigError::Malformed`] for a file that does not parse,
    /// [`ConfigError::Invalid`] for a value that does not make sense.
    pub fn load(dir: &Path) -> Result<Self, ConfigError> {
        let path = dir.join("config.toml");
        let mut config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| ConfigError::Malformed {
                path: path.display().to_string(),
                detail: e.message().to_owned(),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        };

        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    /// Apply `HIVEMIND_*` environment overrides (SPEC §13.1).
    ///
    /// An unparseable value is ignored rather than fatal: an environment
    /// variable is often set by something other than the person running the
    /// daemon, and refusing to start over one would be worse than using the
    /// configured value.
    fn apply_env(&mut self) {
        if let Ok(name) = std::env::var("HIVEMIND_NAME")
            && !name.trim().is_empty()
        {
            self.name = name;
        }
        if let Ok(owner) = std::env::var("HIVEMIND_OWNER") {
            self.owner = Some(owner).filter(|o| !o.trim().is_empty());
        }
        if let Ok(port) = std::env::var("HIVEMIND_PEER_PORT")
            && let Ok(port) = port.parse()
        {
            self.peer_port = port;
        }
        if let Ok(port) = std::env::var("HIVEMIND_LOCAL_PORT")
            && let Ok(port) = port.parse()
        {
            self.local_port = port;
        }
        if let Ok(value) = std::env::var("HIVEMIND_NOTIFICATIONS")
            && let Some(flag) = parse_bool(&value)
        {
            self.notifications = flag;
        }
        if let Ok(value) = std::env::var("HIVEMIND_PREFETCH")
            && let Some(flag) = parse_bool(&value)
        {
            self.prefetch = flag;
        }
    }

    /// Check the values make sense together.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] naming the key and what it should have been.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.name.trim().is_empty() {
            return Err(ConfigError::Invalid {
                key: "name",
                value: "empty".to_owned(),
                expectation: "a node needs a name peers can show a human",
            });
        }
        if self.peer_port == 0 || self.local_port == 0 {
            return Err(ConfigError::Invalid {
                key: "peer_port/local_port",
                value: "0".to_owned(),
                expectation: "port 0 asks the OS to pick, which peers cannot discover",
            });
        }
        if self.peer_port == self.local_port {
            return Err(ConfigError::Invalid {
                key: "local_port",
                value: self.local_port.to_string(),
                expectation: "it must differ from peer_port; one listener is \
                              unauthenticated and the other is not",
            });
        }
        if self.inline_max_bytes > self.max_attachment_bytes {
            return Err(ConfigError::Invalid {
                key: "inline_max_bytes",
                value: self.inline_max_bytes.to_string(),
                expectation: "it cannot exceed max_attachment_bytes, or a file \
                              small enough to inline would be too large to accept",
            });
        }
        Ok(())
    }
}

/// Accept what a person would plausibly type for a boolean.
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_file_is_the_defaults_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config::load(dir.path()).expect("missing file is fine");
        assert_eq!(config.local_port, DEFAULT_LOCAL_PORT);
        assert!(config.notifications);
    }

    #[test]
    fn a_partial_config_file_keeps_the_defaults_for_everything_else() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("config.toml"), b"notifications = false\n").expect("write");

        let config = Config::load(dir.path()).expect("load");
        assert!(!config.notifications);
        assert_eq!(config.peer_port, DEFAULT_PEER_PORT);
    }

    #[test]
    fn an_unknown_key_is_reported_rather_than_ignored() {
        // A typo that silently does nothing is worse than an error: the user
        // thinks they changed a setting and did not.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("config.toml"), b"notifcations = false\n").expect("write");

        assert!(matches!(
            Config::load(dir.path()),
            Err(ConfigError::Malformed { .. })
        ));
    }

    #[test]
    fn a_malformed_file_says_which_file_and_what_was_wrong() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("config.toml"), b"notifications = [[[").expect("write");

        let error = Config::load(dir.path()).expect_err("should refuse");
        assert!(error.to_string().contains("config.toml"), "got: {error}");
    }

    #[test]
    fn the_two_listeners_may_not_share_a_port() {
        // One is unauthenticated loopback, the other is mutual TLS on the
        // network. Collapsing them would be a security bug, not a typo.
        let config = Config {
            peer_port: 8401,
            local_port: 8401,
            ..Config::default()
        };
        let error = config.validate().expect_err("should refuse");
        assert!(
            error.to_string().contains("unauthenticated"),
            "got: {error}"
        );
    }

    #[test]
    fn port_zero_is_refused_because_peers_could_never_find_it() {
        let config = Config {
            peer_port: 0,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn an_empty_name_is_refused() {
        let config = Config {
            name: "   ".to_owned(),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn an_inline_threshold_above_the_size_limit_is_refused() {
        let config = Config {
            inline_max_bytes: DEFAULT_MAX_ATTACHMENT_BYTES + 1,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn defaults_are_valid() {
        // If the defaults do not pass validation, a fresh install cannot start.
        Config::default()
            .validate()
            .expect("defaults must be valid");
    }

    #[test]
    fn booleans_accept_what_a_person_would_type() {
        for yes in ["1", "true", "TRUE", "yes", "on", " true "] {
            assert_eq!(parse_bool(yes), Some(true), "{yes}");
        }
        for no in ["0", "false", "FALSE", "no", "off"] {
            assert_eq!(parse_bool(no), Some(false), "{no}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn a_config_round_trips_through_toml() {
        let config = Config {
            owner: Some("matthew".to_owned()),
            notifications: false,
            ..Config::default()
        };
        let text = toml::to_string_pretty(&config).expect("serialise");
        let back: Config = toml::from_str(&text).expect("deserialise");
        assert_eq!(back, config);
    }
}
