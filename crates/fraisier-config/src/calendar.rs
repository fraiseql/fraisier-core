//! The fraisier-native calendar vocabulary, translated to systemd `OnCalendar=`.
//!
//! `[schedule].calendar` is the **stable** scheduling surface; it deliberately
//! does *not* expose the full `OnCalendar=` grammar (which would leak the systemd
//! implementation and lock fraisier to it forever). The minimal vocabulary is:
//!
//! | fraisier `calendar`     | systemd `OnCalendar=`     |
//! |-------------------------|---------------------------|
//! | `hourly`                | `*-*-* *:00:00`           |
//! | `daily HH:MM`           | `*-*-* HH:MM:00`          |
//! | `weekly DOW HH:MM`      | `DOW *-*-* HH:MM:00`      |
//! | `monthly DD HH:MM`      | `*-*-DD HH:MM:00`         |
//!
//! Power users who need the full grammar opt into the systemd lock knowingly via
//! `[schedule].on_calendar_raw` (registered unstable), not here.

/// Translate a native `calendar` spec into a systemd `OnCalendar=` expression.
///
/// # Errors
/// A human-readable message if `spec` is empty, names an unknown cadence, or
/// carries an out-of-range time / day-of-week / day-of-month.
pub fn to_on_calendar(spec: &str) -> Result<String, String> {
    let tokens: Vec<&str> = spec.split_whitespace().collect();
    let cadence = tokens
        .first()
        .ok_or_else(|| "empty calendar spec".to_owned())?;
    match cadence.to_ascii_lowercase().as_str() {
        "hourly" => {
            expect_arity(&tokens, 1, "hourly")?;
            Ok("*-*-* *:00:00".to_owned())
        }
        "daily" => {
            expect_arity(&tokens, 2, "daily HH:MM")?;
            let (hour, minute) = parse_hh_mm(tokens[1])?;
            Ok(format!("*-*-* {hour:02}:{minute:02}:00"))
        }
        "weekly" => {
            expect_arity(&tokens, 3, "weekly DOW HH:MM")?;
            let dow = parse_dow(tokens[1])?;
            let (hour, minute) = parse_hh_mm(tokens[2])?;
            Ok(format!("{dow} *-*-* {hour:02}:{minute:02}:00"))
        }
        "monthly" => {
            expect_arity(&tokens, 3, "monthly DD HH:MM")?;
            let day = parse_dom(tokens[1])?;
            let (hour, minute) = parse_hh_mm(tokens[2])?;
            Ok(format!("*-*-{day:02} {hour:02}:{minute:02}:00"))
        }
        other => Err(format!(
            "unknown calendar cadence '{other}' (expected: hourly, daily, weekly, monthly)"
        )),
    }
}

/// Require exactly `n` whitespace tokens, naming the expected `shape` otherwise.
fn expect_arity(tokens: &[&str], n: usize, shape: &str) -> Result<(), String> {
    if tokens.len() == n {
        Ok(())
    } else {
        Err(format!("calendar spec must read `{shape}`"))
    }
}

/// Parse and range-check an `HH:MM` clock time.
fn parse_hh_mm(text: &str) -> Result<(u8, u8), String> {
    let (hh, mm) = text
        .split_once(':')
        .ok_or_else(|| format!("'{text}' is not a HH:MM time"))?;
    let hour: u8 = hh
        .parse()
        .map_err(|_| format!("'{hh}' is not a valid hour"))?;
    let minute: u8 = mm
        .parse()
        .map_err(|_| format!("'{mm}' is not a valid minute"))?;
    if hour > 23 {
        return Err(format!("hour {hour} is out of range (00-23)"));
    }
    if minute > 59 {
        return Err(format!("minute {minute} is out of range (00-59)"));
    }
    Ok((hour, minute))
}

/// Canonicalize a day-of-week token to systemd's `Mon`..`Sun`.
fn parse_dow(text: &str) -> Result<&'static str, String> {
    match text.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok("Mon"),
        "tue" | "tuesday" => Ok("Tue"),
        "wed" | "wednesday" => Ok("Wed"),
        "thu" | "thursday" => Ok("Thu"),
        "fri" | "friday" => Ok("Fri"),
        "sat" | "saturday" => Ok("Sat"),
        "sun" | "sunday" => Ok("Sun"),
        _ => Err(format!("'{text}' is not a day of week (Mon..Sun)")),
    }
}

/// Parse and range-check a day-of-month (1-31).
fn parse_dom(text: &str) -> Result<u8, String> {
    let day: u8 = text
        .parse()
        .map_err(|_| format!("'{text}' is not a day of month"))?;
    if (1..=31).contains(&day) {
        Ok(day)
    } else {
        Err(format!("day-of-month {day} is out of range (1-31)"))
    }
}

#[cfg(test)]
mod tests {
    use super::to_on_calendar;

    #[test]
    fn translates_each_cadence() {
        assert_eq!(to_on_calendar("hourly").unwrap(), "*-*-* *:00:00");
        assert_eq!(to_on_calendar("daily 03:00").unwrap(), "*-*-* 03:00:00");
        assert_eq!(to_on_calendar("daily 9:5").unwrap(), "*-*-* 09:05:00");
        assert_eq!(
            to_on_calendar("weekly Mon 09:30").unwrap(),
            "Mon *-*-* 09:30:00"
        );
        assert_eq!(
            to_on_calendar("weekly sunday 23:00").unwrap(),
            "Sun *-*-* 23:00:00"
        );
        assert_eq!(
            to_on_calendar("monthly 1 00:00").unwrap(),
            "*-*-01 00:00:00"
        );
    }

    #[test]
    fn rejects_malformed_specs() {
        for bad in [
            "",
            "fortnightly",
            "daily",       // missing time
            "daily 25:00", // bad hour
            "daily 03:99", // bad minute
            "daily noon",  // not HH:MM
            "weekly Funday 09:00",
            "monthly 0 03:00",  // day < 1
            "monthly 32 03:00", // day > 31
            "hourly 03:00",     // arity
        ] {
            assert!(to_on_calendar(bad).is_err(), "expected error for {bad:?}");
        }
    }
}
