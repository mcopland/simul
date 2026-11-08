//! Text formatting shared by `simul ls` and the sidebar.

/// Compact age: `12s`, `3m`, `2h`, `4d`.
pub fn fmt_age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(fmt_age(0), "0s");
        assert_eq!(fmt_age(59_999), "59s");
        assert_eq!(fmt_age(60_000), "1m");
        assert_eq!(fmt_age(3_599_000), "59m");
        assert_eq!(fmt_age(7_200_000), "2h");
        assert_eq!(fmt_age(90_000_000), "1d");
    }
}
