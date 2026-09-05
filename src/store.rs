use std::error::Error;
use std::fmt;

use lifx::LifxId;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Row};

use crate::registry::LightMode;

static MIGRATOR: Migrator = sqlx::migrate!();

#[derive(Debug, Clone)]
pub struct StoredLight {
    pub id: LifxId,
    pub device_label: Option<String>,
    pub friendly_name: Option<String>,
    pub control_enabled: bool,
    pub mode: LightMode,
}

#[derive(Debug)]
pub enum StoreError {
    Sqlx(sqlx::Error),
    Migrate(sqlx::migrate::MigrateError),
    InvalidLifxId(String),
    InvalidMode(String),
    UnknownLight(LifxId),
    FriendlyNameConflict(String),
    InvalidFriendlyName(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlx(err) => write!(f, "database error: {err}"),
            Self::Migrate(err) => write!(f, "database migration error: {err}"),
            Self::InvalidLifxId(value) => write!(f, "invalid persisted LIFX ID '{value}'"),
            Self::InvalidMode(value) => write!(f, "invalid persisted light mode '{value}'"),
            Self::UnknownLight(id) => write!(f, "unknown persisted light {id:#018x}"),
            Self::FriendlyNameConflict(name) => {
                write!(f, "friendly name '{name}' is already in use")
            }
            Self::InvalidFriendlyName(name) => {
                write!(f, "friendly name '{name}' violates database constraints")
            }
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlx(err) => Some(err),
            Self::Migrate(err) => Some(err),
            Self::InvalidLifxId(_)
            | Self::InvalidMode(_)
            | Self::UnknownLight(_)
            | Self::FriendlyNameConflict(_)
            | Self::InvalidFriendlyName(_) => None,
        }
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(value: sqlx::Error) -> Self {
        Self::Sqlx(value)
    }
}

impl From<sqlx::migrate::MigrateError> for StoreError {
    fn from(value: sqlx::migrate::MigrateError) -> Self {
        Self::Migrate(value)
    }
}

#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        let options = database_url
            .parse::<PgConnectOptions>()?
            // PostgreSQL is a host-local SHOCS service. TLS is unnecessary on
            // the loopback-only database connection and is disabled explicitly.
            .ssl_mode(PgSslMode::Disable)
            .application_name("shocs-lc");

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        MIGRATOR.run(&pool).await?;

        Ok(Self { pool })
    }

    pub async fn load_lights(&self) -> Result<Vec<StoredLight>, StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT lifx_id, device_label, friendly_name, control_enabled, mode
            FROM lights
            ORDER BY lifx_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(decode_light).collect()
    }

    pub async fn group_count(&self) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT count(*) AS count FROM light_groups")
            .fetch_one(&self.pool)
            .await?;

        Ok(row.try_get("count")?)
    }

    /// Ensure a discovered physical light has durable configuration.
    /// Existing mode/name configuration wins; discovery refreshes only the
    /// observed LIFX device label.
    pub async fn upsert_discovered_light(
        &self,
        id: LifxId,
        device_label: Option<&str>,
        default_mode: LightMode,
    ) -> Result<StoredLight, StoreError> {
        let lifx_id = format_lifx_id(id);
        let row = sqlx::query(
            r#"
            INSERT INTO lights (lifx_id, device_label, mode)
            VALUES ($1, $2, $3)
            ON CONFLICT (lifx_id) DO UPDATE
            SET device_label = COALESCE(EXCLUDED.device_label, lights.device_label),
                updated_at = CASE
                    WHEN EXCLUDED.device_label IS NOT NULL
                     AND EXCLUDED.device_label IS DISTINCT FROM lights.device_label
                    THEN now()
                    ELSE lights.updated_at
                END
            RETURNING lifx_id, device_label, friendly_name, control_enabled, mode
            "#,
        )
        .bind(lifx_id)
        .bind(device_label)
        .bind(default_mode.as_str())
        .fetch_one(&self.pool)
        .await?;

        decode_light(row)
    }

    pub async fn set_light_mode(&self, id: LifxId, mode: LightMode) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            UPDATE lights
            SET mode = $2, updated_at = now()
            WHERE lifx_id = $1
            "#,
        )
        .bind(format_lifx_id(id))
        .bind(mode.as_str())
        .execute(&self.pool)
        .await?;

        require_one_row(id, result.rows_affected())
    }

    pub async fn set_friendly_name(
        &self,
        id: LifxId,
        friendly_name: Option<&str>,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            UPDATE lights
            SET friendly_name = $2, updated_at = now()
            WHERE lifx_id = $1
            "#,
        )
        .bind(format_lifx_id(id))
        .bind(friendly_name)
        .execute(&self.pool)
        .await
        .map_err(|err| map_friendly_name_error(err, friendly_name))?;

        require_one_row(id, result.rows_affected())
    }

    pub async fn set_control_enabled(
        &self,
        id: LifxId,
        control_enabled: bool,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            UPDATE lights
            SET control_enabled = $2, updated_at = now()
            WHERE lifx_id = $1
            "#,
        )
        .bind(format_lifx_id(id))
        .bind(control_enabled)
        .execute(&self.pool)
        .await?;

        require_one_row(id, result.rows_affected())
    }

    pub async fn set_device_label(
        &self,
        id: LifxId,
        device_label: Option<&str>,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            UPDATE lights
            SET device_label = $2, updated_at = now()
            WHERE lifx_id = $1
            "#,
        )
        .bind(format_lifx_id(id))
        .bind(device_label)
        .execute(&self.pool)
        .await?;

        require_one_row(id, result.rows_affected())
    }
}

fn map_friendly_name_error(err: sqlx::Error, friendly_name: Option<&str>) -> StoreError {
    if let sqlx::Error::Database(database_error) = &err {
        match database_error.code().as_deref() {
            // unique_violation
            Some("23505") => {
                return StoreError::FriendlyNameConflict(
                    friendly_name.unwrap_or_default().to_string(),
                );
            }
            // check_violation
            Some("23514") => {
                return StoreError::InvalidFriendlyName(
                    friendly_name.unwrap_or_default().to_string(),
                );
            }
            _ => {}
        }
    }

    StoreError::Sqlx(err)
}

fn require_one_row(id: LifxId, rows_affected: u64) -> Result<(), StoreError> {
    if rows_affected != 1 {
        return Err(StoreError::UnknownLight(id));
    }

    Ok(())
}

fn decode_light(row: sqlx::postgres::PgRow) -> Result<StoredLight, StoreError> {
    let lifx_id: String = row.try_get("lifx_id")?;
    let mode: String = row.try_get("mode")?;

    let id = u64::from_str_radix(&lifx_id, 16)
        .map_err(|_| StoreError::InvalidLifxId(lifx_id.clone()))?;
    let mode = LightMode::from_str(&mode).ok_or_else(|| StoreError::InvalidMode(mode.clone()))?;

    Ok(StoredLight {
        id,
        device_label: row.try_get("device_label")?,
        friendly_name: row.try_get("friendly_name")?,
        control_enabled: row.try_get("control_enabled")?,
        mode,
    })
}

fn format_lifx_id(id: LifxId) -> String {
    format!("{id:016x}")
}
