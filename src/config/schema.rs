//! Configuration schema validation and helpers

use std::time::Duration;

/// Parse a duration string like "30m", "1h", "2h30m"
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let mut total_seconds: u64 = 0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else {
            let num: u64 = current_num
                .parse()
                .map_err(|_| format!("Invalid number in duration: {}", s))?;
            current_num.clear();

            total_seconds += match c {
                's' => num,
                'm' => num * 60,
                'h' => num * 3600,
                'd' => num * 86400,
                _ => return Err(format!("Unknown duration unit: {}", c)),
            };
        }
    }

    if total_seconds == 0 {
        return Err(format!("Invalid duration: {}", s));
    }

    Ok(Duration::from_secs(total_seconds))
}

/// Parse a time string like "09:00" or "22:30"
pub fn parse_time(s: &str) -> Result<(u8, u8), String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return Err(format!("Invalid time format: {}. Expected HH:MM", s));
    }

    let hour: u8 = parts[0]
        .parse()
        .map_err(|_| format!("Invalid hour: {}", parts[0]))?;
    let minute: u8 = parts[1]
        .parse()
        .map_err(|_| format!("Invalid minute: {}", parts[1]))?;

    if hour > 23 {
        return Err(format!("Hour must be 0-23, got: {}", hour));
    }
    if minute > 59 {
        return Err(format!("Minute must be 0-59, got: {}", minute));
    }

    Ok((hour, minute))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn test_parse_time() {
        assert_eq!(parse_time("09:00").unwrap(), (9, 0));
        assert_eq!(parse_time("22:30").unwrap(), (22, 30));
        assert_eq!(parse_time("00:00").unwrap(), (0, 0));
        assert_eq!(parse_time("23:59").unwrap(), (23, 59));
    }
}
