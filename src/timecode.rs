//! Positions in a video: `90`, `12:30` or `1:02:03` on the way in, `H:MM:SS` on the way out.

/// Reads seconds from `SS`, `MM:SS` or `HH:MM:SS` (fractions allowed on the seconds).
pub fn parse(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut seconds = 0.0;
    for part in parts {
        let value: f64 = part.parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        seconds = seconds * 60.0 + value;
    }
    Some(seconds)
}

/// Writes seconds as `H:MM:SS`, the form UPnP expects and people read.
pub fn format(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds as u64
    } else {
        0
    };
    format!("{}:{:02}:{:02}", total / 3600, total / 60 % 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(parse("90"), Some(90.0));
        assert_eq!(parse("12:30"), Some(750.0));
        assert_eq!(parse("1:02:03.5"), Some(3723.5));
        assert_eq!(parse(" 0:00:12 "), Some(12.0));
        for bad in ["", "a", "1:2:3:4", "-5", "NOT_IMPLEMENTED", "1::2", "inf"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn formats() {
        assert_eq!(format(0.0), "0:00:00");
        assert_eq!(format(3723.9), "1:02:03");
        assert_eq!(format(-4.0), "0:00:00");
        assert_eq!(format(f64::NAN), "0:00:00");
    }
}
