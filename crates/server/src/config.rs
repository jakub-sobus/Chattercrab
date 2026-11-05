//! Server configuration loaded from `CHATTERCRAB_*` environment variables.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

/// Placeholder value written by the env template; treated as "no TURN secret".
pub const TURN_SECRET_PLACEHOLDER: &str = "__GENERATED_BY_INSTALL_SH__";

/// Paths of the PEM certificate chain and private key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPaths {
    /// Certificate chain (`fullchain.pem`).
    pub cert: PathBuf,
    /// Private key (`privkey.pem`).
    pub key: PathBuf,
}

/// TURN REST API settings.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnConfig {
    /// Shared secret, identical to Coturn's `static-auth-secret`.
    pub secret: String,
    /// TURN realm (informational; Coturn enforces it).
    pub realm: String,
    /// ICE server URLs handed to clients.
    pub urls: Vec<String>,
}

impl std::fmt::Debug for TurnConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnConfig")
            .field("secret", &"<redacted>")
            .field("realm", &self.realm)
            .field("urls", &self.urls)
            .finish()
    }
}

/// Complete server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Listen address (`CHATTERCRAB_BIND`).
    pub bind: SocketAddr,
    /// TLS files; `None` serves plain HTTP (local development).
    pub tls: Option<TlsPaths>,
    /// TURN settings; `None` makes `/turn-credentials` answer `503`.
    pub turn: Option<TurnConfig>,
    /// Maximum members per room (`CHATTERCRAB_MAX_ROOM_SIZE`).
    pub max_room_size: usize,
    /// Maximum number of rooms (`CHATTERCRAB_MAX_ROOMS`).
    pub max_rooms: usize,
    /// Empty rooms idle longer than this are evicted (`CHATTERCRAB_ROOM_IDLE_TTL_SECS`).
    pub room_idle_ttl: Duration,
    /// Time allowed between connect and `join`.
    pub join_timeout: Duration,
    /// Interval between WebSocket Ping frames.
    pub ping_interval: Duration,
    /// Interval of the idle-room sweeper.
    pub sweep_interval: Duration,
    /// Maximum time between a shutdown request and process exit.
    pub shutdown_grace: Duration,
    /// Sustained `POST /rooms` rate per IP, per minute.
    pub create_room_per_minute: NonZeroU32,
    /// `POST /rooms` burst per IP.
    pub create_room_burst: NonZeroU32,
    /// Sustained `GET /turn-credentials` rate per IP, per minute.
    pub turn_per_minute: NonZeroU32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 8443)),
            tls: None,
            turn: None,
            max_room_size: 5,
            max_rooms: 100,
            room_idle_ttl: Duration::from_secs(3600),
            join_timeout: Duration::from_secs(5),
            ping_interval: Duration::from_secs(20),
            sweep_interval: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(10),
            create_room_per_minute: NonZeroU32::MIN.saturating_add(9),
            create_room_burst: NonZeroU32::MIN.saturating_add(4),
            turn_per_minute: NonZeroU32::MIN.saturating_add(29),
        }
    }
}

/// A configuration value could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A variable holds an unparsable or out-of-range value.
    #[error("{var}={value:?} is invalid: {reason}")]
    Invalid {
        /// Variable name.
        var: &'static str,
        /// Offending value.
        value: String,
        /// Explanation.
        reason: &'static str,
    },
    /// Only one of the TLS certificate and key paths was set.
    #[error("CHATTERCRAB_TLS_CERT and CHATTERCRAB_TLS_KEY must be set together")]
    TlsIncomplete,
}

impl Config {
    /// Loads the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Loads the configuration through `lookup`, which maps a variable name to its
    /// value. Empty values count as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| {
            lookup(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let mut config = Self::default();

        if let Some(v) = get("CHATTERCRAB_BIND") {
            config.bind = v.parse().map_err(|_| ConfigError::Invalid {
                var: "CHATTERCRAB_BIND",
                value: v.clone(),
                reason: "expected host:port, e.g. 0.0.0.0:8443",
            })?;
        }

        config.tls = match (get("CHATTERCRAB_TLS_CERT"), get("CHATTERCRAB_TLS_KEY")) {
            (Some(cert), Some(key)) => Some(TlsPaths {
                cert: cert.into(),
                key: key.into(),
            }),
            (None, None) => None,
            _ => return Err(ConfigError::TlsIncomplete),
        };

        let secret = get("CHATTERCRAB_TURN_SECRET").filter(|s| s != TURN_SECRET_PLACEHOLDER);
        let urls: Vec<String> = get("CHATTERCRAB_TURN_URLS")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        config.turn = match secret {
            Some(secret) if !urls.is_empty() => Some(TurnConfig {
                secret,
                realm: get("CHATTERCRAB_TURN_REALM").unwrap_or_default(),
                urls,
            }),
            _ => None,
        };

        if let Some(v) = get("CHATTERCRAB_MAX_ROOM_SIZE") {
            config.max_room_size = parse_positive("CHATTERCRAB_MAX_ROOM_SIZE", &v)?;
        }
        if let Some(v) = get("CHATTERCRAB_MAX_ROOMS") {
            config.max_rooms = parse_positive("CHATTERCRAB_MAX_ROOMS", &v)?;
        }
        if let Some(v) = get("CHATTERCRAB_ROOM_IDLE_TTL_SECS") {
            let secs = parse_positive("CHATTERCRAB_ROOM_IDLE_TTL_SECS", &v)?;
            config.room_idle_ttl = Duration::from_secs(secs as u64);
        }
        Ok(config)
    }
}

fn parse_positive(var: &'static str, value: &str) -> Result<usize, ConfigError> {
    match value.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(ConfigError::Invalid {
            var,
            value: value.to_owned(),
            reason: "expected a positive integer",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults_when_empty() {
        let c = load(&[]).unwrap();
        assert_eq!(c.bind, "0.0.0.0:8443".parse().unwrap());
        assert!(c.tls.is_none());
        assert!(c.turn.is_none());
        assert_eq!(c.max_room_size, 5);
        assert_eq!(c.max_rooms, 100);
        assert_eq!(c.room_idle_ttl, Duration::from_secs(3600));
        assert_eq!(c.create_room_per_minute.get(), 10);
        assert_eq!(c.create_room_burst.get(), 5);
        assert_eq!(c.turn_per_minute.get(), 30);
    }

    #[test]
    fn parses_full_template() {
        let c = load(&[
            ("CHATTERCRAB_BIND", "127.0.0.1:9000"),
            ("CHATTERCRAB_TLS_CERT", "/etc/chattercrab/tls/fullchain.pem"),
            ("CHATTERCRAB_TLS_KEY", "/etc/chattercrab/tls/privkey.pem"),
            ("CHATTERCRAB_TURN_SECRET", "abc"),
            ("CHATTERCRAB_TURN_REALM", "chat.example.com"),
            (
                "CHATTERCRAB_TURN_URLS",
                "turn:a:3478?transport=udp, turns:a:5349?transport=tcp",
            ),
            ("CHATTERCRAB_MAX_ROOM_SIZE", "4"),
            ("CHATTERCRAB_MAX_ROOMS", "7"),
            ("CHATTERCRAB_ROOM_IDLE_TTL_SECS", "60"),
        ])
        .unwrap();
        assert_eq!(c.bind.port(), 9000);
        assert!(c.tls.is_some());
        let turn = c.turn.unwrap();
        assert_eq!(turn.urls.len(), 2);
        assert_eq!(turn.realm, "chat.example.com");
        assert_eq!(c.max_room_size, 4);
        assert_eq!(c.max_rooms, 7);
        assert_eq!(c.room_idle_ttl, Duration::from_secs(60));
    }

    #[test]
    fn placeholder_secret_disables_turn() {
        let c = load(&[
            ("CHATTERCRAB_TURN_SECRET", TURN_SECRET_PLACEHOLDER),
            ("CHATTERCRAB_TURN_URLS", "turn:a:3478"),
        ])
        .unwrap();
        assert!(c.turn.is_none());
    }

    #[test]
    fn rejects_bad_values() {
        assert!(load(&[("CHATTERCRAB_BIND", "nope")]).is_err());
        assert!(load(&[("CHATTERCRAB_MAX_ROOMS", "0")]).is_err());
        assert!(load(&[("CHATTERCRAB_MAX_ROOM_SIZE", "-1")]).is_err());
        assert!(matches!(
            load(&[("CHATTERCRAB_TLS_CERT", "/x")]),
            Err(ConfigError::TlsIncomplete)
        ));
    }

    #[test]
    fn debug_redacts_secret() {
        let t = TurnConfig {
            secret: "topsecret".into(),
            realm: String::new(),
            urls: vec![],
        };
        assert!(!format!("{t:?}").contains("topsecret"));
    }
}
