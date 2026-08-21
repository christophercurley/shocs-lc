use chrono::{
    DateTime, Duration as ChronoDuration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Utc,
};
use chrono_tz::Tz;
use lifx::Power;

pub fn desired_power_now(timezone: Tz, off_time: NaiveTime, on_time: NaiveTime) -> Power {
    desired_power_at(Utc::now().with_timezone(&timezone), off_time, on_time)
}

pub fn desired_power_at(now: DateTime<Tz>, off_time: NaiveTime, on_time: NaiveTime) -> Power {
    let time = now.time();

    let is_off = if off_time < on_time {
        time >= off_time && time < on_time
    } else if off_time > on_time {
        time >= off_time || time < on_time
    } else {
        false
    };

    if is_off { Power::Off } else { Power::On }
}

pub fn next_power_boundary(
    timezone: Tz,
    off_time: NaiveTime,
    on_time: NaiveTime,
) -> Result<(DateTime<Tz>, Power), String> {
    let now = Utc::now().with_timezone(&timezone);

    let next_off = next_occurrence(now, off_time)?;
    let next_on = next_occurrence(now, on_time)?;

    if next_off <= next_on {
        Ok((next_off, Power::Off))
    } else {
        Ok((next_on, Power::On))
    }
}

fn next_occurrence(now: DateTime<Tz>, time: NaiveTime) -> Result<DateTime<Tz>, String> {
    let timezone = now.timezone();
    let mut date = now.date_naive();
    let mut candidate = resolve_local_datetime(timezone, date, time)?;

    if candidate <= now {
        date = date
            .succ_opt()
            .ok_or_else(|| "could not advance schedule date".to_string())?;
        candidate = resolve_local_datetime(timezone, date, time)?;
    }

    Ok(candidate)
}

fn resolve_local_datetime(
    timezone: Tz,
    date: NaiveDate,
    time: NaiveTime,
) -> Result<DateTime<Tz>, String> {
    let mut naive: NaiveDateTime = date.and_time(time);

    // Spring-forward can make a configured local time nonexistent.
    // Walk forward to the first valid local instant. For fall-back
    // ambiguity, use the earlier occurrence.
    for _ in 0..180 {
        match timezone.from_local_datetime(&naive) {
            LocalResult::Single(value) => return Ok(value),
            LocalResult::Ambiguous(first, second) => return Ok(first.min(second)),
            LocalResult::None => naive += ChronoDuration::minutes(1),
        }
    }

    Err(format!(
        "could not resolve local schedule time {date} {time} in {timezone}"
    ))
}
