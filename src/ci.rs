//! CI detection. Every behavior that changes under CI asks here.

/// True when `CI` is set to anything but an explicit off value.
pub fn is_ci() -> bool {
    std::env::var("CI").map_or(false, |value| enabled(&value))
}

fn enabled(value: &str) -> bool {
    !matches!(value.trim().to_ascii_lowercase().as_str(), "" | "0" | "false")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_values_are_not_ci() {
        for value in ["", " ", "0", "false", "FALSE"] {
            assert!(!enabled(value), "{:?}", value);
        }
        for value in ["true", "1", "yes", "github"] {
            assert!(enabled(value), "{:?}", value);
        }
    }
}
