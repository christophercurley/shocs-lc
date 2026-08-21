use std::env;
use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use chrono::NaiveTime;
use chrono_tz::Tz;
use lifx::LifxId;

const DEFAULT_DISCOVERY_INTERVAL_SECS: u64 = 30;
const DEFAULT_COLOR_INTERVAL_SECS: u64 = 10 * 60;
const DEFAULT_TRANSITION_SECS: u64 = 5;
const DEFAULT_TIMEZONE: &str = "America/New_York";
const DEFAULT_OFF_TIME: &str = "02:00";
const DEFAULT_ON_TIME: &str = "10:00";

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub lifx_broadcast_addr: SocketAddr,
    pub controlled_ids: Vec<LifxId>,
    pub discovery_interval: Duration,
    pub color_interval: Duration,
    pub transition: Duration,
    pub timezone: Tz,
    pub off_time: NaiveTime,
    pub on_time: NaiveTime,
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
        let controlled_ids = required_lifx_ids("SHOCS_LC_CONTROLLED_IDS")?;

        let discovery_interval = Duration::from_secs(optional_u64(
            "SHOCS_LC_DISCOVERY_INTERVAL_SECS",
            DEFAULT_DISCOVERY_INTERVAL_SECS,
        )?);

        let color_interval = Duration::from_secs(optional_u64(
            "SHOCS_LC_COLOR_INTERVAL_SECS",
            DEFAULT_COLOR_INTERVAL_SECS,
        )?);

        let transition = Duration::from_secs(optional_u64(
            "SHOCS_LC_TRANSITION_SECS",
            DEFAULT_TRANSITION_SECS,
        )?);

        let timezone_name =
            env::var("SHOCS_LC_TIMEZONE").unwrap_or_else(|_| DEFAULT_TIMEZONE.to_string());

        let timezone = timezone_name
            .parse::<Tz>()
            .map_err(|_| ConfigError(format!("invalid SHOCS_LC_TIMEZONE '{timezone_name}'")))?;

        let off_time = optional_time("SHOCS_LC_OFF_TIME", DEFAULT_OFF_TIME)?;
        let on_time = optional_time("SHOCS_LC_ON_TIME", DEFAULT_ON_TIME)?;

        if discovery_interval.is_zero() {
            return Err(ConfigError(
                "SHOCS_LC_DISCOVERY_INTERVAL_SECS must be greater than zero".into(),
            ));
        }

        if color_interval.is_zero() {
            return Err(ConfigError(
                "SHOCS_LC_COLOR_INTERVAL_SECS must be greater than zero".into(),
            ));
        }

        Ok(Self {
            bind_addr,
            lifx_broadcast_addr,
            controlled_ids,
            discovery_interval,
            color_interval,
            transition,
            timezone,
            off_time,
            on_time,
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

fn optional_u64(name: &str, default: u64) -> Result<u64, ConfigError> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|err| ConfigError(format!("invalid {name} '{value}': {err}"))),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(ConfigError(format!("could not read {name}: {err}"))),
    }
}

fn optional_time(name: &str, default: &str) -> Result<NaiveTime, ConfigError> {
    let value = env::var(name).unwrap_or_else(|_| default.to_string());

    NaiveTime::parse_from_str(&value, "%H:%M")
        .map_err(|err| ConfigError(format!("invalid {name} '{value}': {err}")))
}

fn required_lifx_ids(name: &str) -> Result<Vec<LifxId>, ConfigError> {
    let value = env::var(name)
        .map_err(|_| ConfigError(format!("required environment variable {name} is not set")))?;

    let ids = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_lifx_id)
        .collect::<Result<Vec<_>, _>>()?;

    if ids.is_empty() {
        return Err(ConfigError(format!(
            "{name} must contain at least one LIFX ID"
        )));
    }

    Ok(ids)
}

fn parse_lifx_id(value: &str) -> Result<LifxId, ConfigError> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);

    u64::from_str_radix(hex, 16)
        .map_err(|err| ConfigError(format!("invalid LIFX ID '{value}': {err}")))
}
