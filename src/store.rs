use std::error::Error;
use std::fmt;

use chrono::NaiveTime;
use chrono_tz::Tz;
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

#[derive(Debug, Clone)]
pub struct StoredGroup {
    pub id: i64,
    pub name: String,
    pub member_ids: Vec<LifxId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredTimerTarget {
    Light(LifxId),
    Group(i64),
}

#[derive(Debug, Clone)]
pub struct StoredTimerSchedule {
    pub id: i64,
    pub target: StoredTimerTarget,
    pub on_time: NaiveTime,
    pub off_time: NaiveTime,
    pub timezone: Tz,
    pub enabled: bool,
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
    UnknownGroup(i64),
    GroupNameConflict(String),
    InvalidGroupName(String),
    UnknownTimerSchedule(i64),
    TimerTargetConflict(String),
    InvalidTimerTarget(String),
    InvalidTimerTimezone(String),
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
            Self::UnknownGroup(id) => write!(f, "unknown light group {id}"),
            Self::GroupNameConflict(name) => {
                write!(f, "group name '{name}' is already in use")
            }
            Self::InvalidGroupName(name) => {
                write!(f, "group name '{name}' violates database constraints")
            }
            Self::UnknownTimerSchedule(id) => write!(f, "unknown timer schedule {id}"),
            Self::TimerTargetConflict(target) => {
                write!(f, "timer target '{target}' already has a schedule")
            }
            Self::InvalidTimerTarget(target) => write!(f, "invalid timer target '{target}'"),
            Self::InvalidTimerTimezone(value) => write!(f, "invalid timer timezone '{value}'"),
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
            | Self::InvalidFriendlyName(_)
            | Self::UnknownGroup(_)
            | Self::GroupNameConflict(_)
            | Self::InvalidGroupName(_)
            | Self::UnknownTimerSchedule(_)
            | Self::TimerTargetConflict(_)
            | Self::InvalidTimerTarget(_)
            | Self::InvalidTimerTimezone(_) => None,
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

    pub async fn load_groups(&self) -> Result<Vec<StoredGroup>, StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT g.id, g.name, m.lifx_id
            FROM light_groups g
            LEFT JOIN light_group_members m ON m.group_id = g.id
            ORDER BY lower(g.name), g.id, m.lifx_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut groups = Vec::<StoredGroup>::new();
        let mut positions = std::collections::HashMap::<i64, usize>::new();

        for row in rows {
            let group_id: i64 = row.try_get("id")?;
            let index = if let Some(index) = positions.get(&group_id).copied() {
                index
            } else {
                let index = groups.len();
                groups.push(StoredGroup {
                    id: group_id,
                    name: row.try_get("name")?,
                    member_ids: Vec::new(),
                });
                positions.insert(group_id, index);
                index
            };

            if let Some(lifx_id) = row.try_get::<Option<String>, _>("lifx_id")? {
                groups[index].member_ids.push(parse_lifx_id(&lifx_id)?);
            }
        }

        Ok(groups)
    }

    pub async fn load_timer_schedules(&self) -> Result<Vec<StoredTimerSchedule>, StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT
                id,
                target_type,
                light_id,
                group_id,
                to_char(on_time, 'HH24:MI') AS on_time,
                to_char(off_time, 'HH24:MI') AS off_time,
                timezone,
                enabled
            FROM timer_schedules
            ORDER BY id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(decode_timer_schedule).collect()
    }

    pub async fn create_group(&self, name: &str) -> Result<StoredGroup, StoreError> {
        let row = sqlx::query(
            r#"
            INSERT INTO light_groups (name)
            VALUES ($1)
            RETURNING id, name
            "#,
        )
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| map_group_name_error(err, name))?;

        Ok(StoredGroup {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            member_ids: Vec::new(),
        })
    }

    pub async fn rename_group(&self, id: i64, name: &str) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            UPDATE light_groups
            SET name = $2, updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(|err| map_group_name_error(err, name))?;

        require_one_group(id, result.rows_affected())
    }

    pub async fn delete_group(&self, id: i64) -> Result<(), StoreError> {
        let result = sqlx::query("DELETE FROM light_groups WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;

        require_one_group(id, result.rows_affected())
    }

    pub async fn set_group_members(
        &self,
        id: i64,
        member_ids: &[LifxId],
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;

        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM light_groups WHERE id = $1)")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;

        if !exists {
            return Err(StoreError::UnknownGroup(id));
        }

        sqlx::query("DELETE FROM light_group_members WHERE group_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        for member_id in member_ids {
            sqlx::query(
                r#"
                INSERT INTO light_group_members (group_id, lifx_id)
                VALUES ($1, $2)
                "#,
            )
            .bind(id)
            .bind(format_lifx_id(*member_id))
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query("UPDATE light_groups SET updated_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
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

    /// Persist one mode across every current member of a SHOCS light group.
    ///
    /// This is a configuration operation, so offline members are updated too.
    /// Physical synchronization is deliberately handled by the controller/web
    /// layer after the transaction commits.
    pub async fn set_group_light_mode(
        &self,
        group_id: i64,
        mode: LightMode,
    ) -> Result<Vec<LifxId>, StoreError> {
        let rows = sqlx::query(
            r#"
            UPDATE lights
            SET mode = $2, updated_at = now()
            WHERE lifx_id IN (
                SELECT lifx_id
                FROM light_group_members
                WHERE group_id = $1
            )
            RETURNING lifx_id
            "#,
        )
        .bind(group_id)
        .bind(mode.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let lifx_id: String = row.try_get("lifx_id")?;
                parse_lifx_id(&lifx_id)
            })
            .collect()
    }

    pub async fn create_timer_schedule(
        &self,
        target: StoredTimerTarget,
        on_time: NaiveTime,
        off_time: NaiveTime,
        timezone: Tz,
        enabled: bool,
    ) -> Result<StoredTimerSchedule, StoreError> {
        let (target_type, light_id, group_id, target_key) = timer_target_parts(target);
        let row = sqlx::query(
            r#"
            INSERT INTO timer_schedules (
                target_type, light_id, group_id, on_time, off_time, timezone, enabled
            )
            VALUES (
                $1, $2, $3, CAST($4 AS time), CAST($5 AS time), $6, $7
            )
            RETURNING
                id,
                target_type,
                light_id,
                group_id,
                to_char(on_time, 'HH24:MI') AS on_time,
                to_char(off_time, 'HH24:MI') AS off_time,
                timezone,
                enabled
            "#,
        )
        .bind(target_type)
        .bind(light_id)
        .bind(group_id)
        .bind(on_time.format("%H:%M").to_string())
        .bind(off_time.format("%H:%M").to_string())
        .bind(timezone.to_string())
        .bind(enabled)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| map_timer_error(err, &target_key))?;

        decode_timer_schedule(row)
    }

    pub async fn update_timer_schedule(
        &self,
        id: i64,
        target: StoredTimerTarget,
        on_time: NaiveTime,
        off_time: NaiveTime,
        timezone: Tz,
        enabled: bool,
    ) -> Result<StoredTimerSchedule, StoreError> {
        let (target_type, light_id, group_id, target_key) = timer_target_parts(target);
        let row = sqlx::query(
            r#"
            UPDATE timer_schedules
            SET target_type = $2,
                light_id = $3,
                group_id = $4,
                on_time = CAST($5 AS time),
                off_time = CAST($6 AS time),
                timezone = $7,
                enabled = $8,
                updated_at = now()
            WHERE id = $1
            RETURNING
                id,
                target_type,
                light_id,
                group_id,
                to_char(on_time, 'HH24:MI') AS on_time,
                to_char(off_time, 'HH24:MI') AS off_time,
                timezone,
                enabled
            "#,
        )
        .bind(id)
        .bind(target_type)
        .bind(light_id)
        .bind(group_id)
        .bind(on_time.format("%H:%M").to_string())
        .bind(off_time.format("%H:%M").to_string())
        .bind(timezone.to_string())
        .bind(enabled)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| map_timer_error(err, &target_key))?;

        let Some(row) = row else {
            return Err(StoreError::UnknownTimerSchedule(id));
        };

        decode_timer_schedule(row)
    }

    pub async fn delete_timer_schedule(&self, id: i64) -> Result<(), StoreError> {
        let result = sqlx::query("DELETE FROM timer_schedules WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() != 1 {
            return Err(StoreError::UnknownTimerSchedule(id));
        }

        Ok(())
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

fn timer_target_parts(
    target: StoredTimerTarget,
) -> (&'static str, Option<String>, Option<i64>, String) {
    match target {
        StoredTimerTarget::Light(id) => (
            "light",
            Some(format_lifx_id(id)),
            None,
            format!("light:{id:016x}"),
        ),
        StoredTimerTarget::Group(id) => ("group", None, Some(id), format!("group:{id}")),
    }
}

fn map_timer_error(err: sqlx::Error, target: &str) -> StoreError {
    if let sqlx::Error::Database(database_error) = &err {
        match database_error.code().as_deref() {
            Some("23505") => return StoreError::TimerTargetConflict(target.to_string()),
            Some("23514") | Some("23503") => {
                return StoreError::InvalidTimerTarget(target.to_string());
            }
            _ => {}
        }
    }

    StoreError::Sqlx(err)
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

fn map_group_name_error(err: sqlx::Error, name: &str) -> StoreError {
    if let sqlx::Error::Database(database_error) = &err {
        match database_error.code().as_deref() {
            Some("23505") => return StoreError::GroupNameConflict(name.to_string()),
            Some("23514") => return StoreError::InvalidGroupName(name.to_string()),
            _ => {}
        }
    }

    StoreError::Sqlx(err)
}

fn require_one_group(id: i64, rows_affected: u64) -> Result<(), StoreError> {
    if rows_affected != 1 {
        return Err(StoreError::UnknownGroup(id));
    }

    Ok(())
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

    let id = parse_lifx_id(&lifx_id)?;
    let mode = LightMode::from_str(&mode).ok_or_else(|| StoreError::InvalidMode(mode.clone()))?;

    Ok(StoredLight {
        id,
        device_label: row.try_get("device_label")?,
        friendly_name: row.try_get("friendly_name")?,
        control_enabled: row.try_get("control_enabled")?,
        mode,
    })
}

fn decode_timer_schedule(row: sqlx::postgres::PgRow) -> Result<StoredTimerSchedule, StoreError> {
    let target_type: String = row.try_get("target_type")?;
    let target = match target_type.as_str() {
        "light" => {
            let value: String = row
                .try_get::<Option<String>, _>("light_id")?
                .ok_or_else(|| StoreError::InvalidTimerTarget("light:<null>".to_string()))?;
            StoredTimerTarget::Light(parse_lifx_id(&value)?)
        }
        "group" => {
            let id = row
                .try_get::<Option<i64>, _>("group_id")?
                .ok_or_else(|| StoreError::InvalidTimerTarget("group:<null>".to_string()))?;
            StoredTimerTarget::Group(id)
        }
        other => return Err(StoreError::InvalidTimerTarget(other.to_string())),
    };

    let on_time: String = row.try_get("on_time")?;
    let off_time: String = row.try_get("off_time")?;
    let timezone_name: String = row.try_get("timezone")?;

    let on_time = NaiveTime::parse_from_str(&on_time, "%H:%M")
        .map_err(|_| StoreError::InvalidTimerTarget(format!("on_time:{on_time}")))?;
    let off_time = NaiveTime::parse_from_str(&off_time, "%H:%M")
        .map_err(|_| StoreError::InvalidTimerTarget(format!("off_time:{off_time}")))?;
    let timezone = timezone_name
        .parse::<Tz>()
        .map_err(|_| StoreError::InvalidTimerTimezone(timezone_name.clone()))?;

    Ok(StoredTimerSchedule {
        id: row.try_get("id")?,
        target,
        on_time,
        off_time,
        timezone,
        enabled: row.try_get("enabled")?,
    })
}

fn parse_lifx_id(value: &str) -> Result<LifxId, StoreError> {
    u64::from_str_radix(value, 16).map_err(|_| StoreError::InvalidLifxId(value.to_string()))
}

fn format_lifx_id(id: LifxId) -> String {
    format!("{id:016x}")
}
