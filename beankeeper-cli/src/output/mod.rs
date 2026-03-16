//! Output formatting: table, JSON, and CSV rendering.
//!
//! Each sub-module renders a specific output format. The [`should_use_color`]
//! function determines whether ANSI colour codes should be emitted based on
//! terminal state and user flags.

pub mod csv;
pub mod json;
pub mod table;

use std::io::IsTerminal;

/// Determine if colors should be used based on environment and flags.
///
/// Colors are disabled when any of the following are true:
/// - `no_color_flag` is `true` (`--no-color` was passed)
/// - The `NO_COLOR` environment variable is set (any value, per <https://no-color.org>)
/// - `TERM=dumb`
/// - stdout is not a terminal (piped / redirected output)
#[must_use]
pub fn should_use_color(no_color_flag: bool) -> bool {
    if no_color_flag {
        return false;
    }
    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }
    if std::env::var("TERM").ok().as_deref() == Some("dumb") {
        return false;
    }
    std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_flag_disables_color() {
        assert!(!should_use_color(true));
    }
}
