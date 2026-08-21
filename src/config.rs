use std::env;
use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

const DEFAULT_DISCOVERY_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub lifx_broadcast_addr: SocketAddr,
    pub discovery_interval: Duration,
}

#[derive(Debug)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ConfigError {}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr = required_socket_addr("SHOCS_LC_BIND")?;
        let lifx_broadcast_addr = required_socket_addr("SHOCS_LC_LIFX_BROADCAST")?;

        let discovery_interval_secs = match env::var("SHOCS_LC_DISCOVERY_INTERVAL_SECS") {
            Ok(value) => value.parse::<u64>().map_err(|err| {
                ConfigError(format!(
                    "invalid SHOCS_LC_DISCOVERY_INTERVAL_SECS '{value}': {err}"
                ))
            })?,
            Err(env::VarError::NotPresent) => DEFAULT_DISCOVERY_INTERVAL_SECS,
            Err(err) => {
                return Err(ConfigError(format!(
                    "could not read SHOCS_LC_DISCOVERY_INTERVAL_SECS: {err}"
                )));
            }
        };

        if discovery_interval_secs == 0 {
            return Err(ConfigError(
                "SHOCS_LC_DISCOVERY_INTERVAL_SECS must be greater than zero".into(),
            ));
        }

        Ok(Self {
            bind_addr,
            lifx_broadcast_addr,
            discovery_interval: Duration::from_secs(discovery_interval_secs),
        })
    }
}

fn required_socket_addr(name: &str) -> Result<SocketAddr, ConfigError> {
    let value = env::var(name)
        .map_err(|_| ConfigError(format!("required environment variable {name} is not set")))?;

    value
        .parse::<SocketAddr>()
        .map_err(|err| ConfigError(format!("invalid {name} '{value}': {err}")))
}
