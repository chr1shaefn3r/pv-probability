//! The page around every report: a self-contained HTML document with a light and a dark
//! theme, the shared palette, and the way numbers are written.
//!
//! * [`layout`] is the shell - the theme switch, the page chrome, the metadata card and
//!   the History block - that every report wraps its own body in.
//! * [`color`] holds the colour scales a report may draw with, as CSS custom properties.
//! * [`format`](mod@format) writes numbers the way every report states them, and [`axis`] picks
//!   gridlines.
//!
//! Like `ha-recorder`, nothing here knows what question a tool answers: a change here
//! rebuilds every tool.

pub mod axis;
pub mod color;
pub mod format;
pub mod layout;

pub use format::{
    escape, format_duration, format_kwh, format_money, format_percent, format_watts, format_years,
};

/// The command that opens a finished report, ready to copy and paste.
///
/// The path is made absolute, so the line still works from a different directory, and
/// quoted when it contains anything a shell would split on.
pub fn open_command(path: &std::path::Path) -> String {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "start"
    } else {
        "xdg-open"
    };
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    format!("{opener} {}", shell_quote(&absolute.display().to_string()))
}

/// Wrap a path in double quotes if a shell would otherwise mangle it.
fn shell_quote(text: &str) -> String {
    if text
        .chars()
        .all(|character| character.is_alphanumeric() || "/._-+=:@~".contains(character))
    {
        text.to_string()
    } else {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_command_is_ready_to_paste() {
        let command = open_command(std::path::Path::new("/tmp/pv-probability.html"));
        assert!(
            command.ends_with(" /tmp/pv-probability.html"),
            "unexpected {command}"
        );
        assert!(
            ["open", "start", "xdg-open"].contains(&command.split(' ').next().unwrap()),
            "unknown opener in {command}"
        );
    }

    #[test]
    fn the_open_command_makes_the_path_absolute() {
        let command = open_command(std::path::Path::new("report.html"));
        let path = command.split_once(' ').expect("an opener and a path").1;
        assert!(
            path.starts_with('/') || path.contains(':'),
            "expected an absolute path, got {path}"
        );
        assert!(path.ends_with("report.html"), "{path}");
    }

    #[test]
    fn the_open_command_quotes_awkward_paths() {
        let command = open_command(std::path::Path::new("/tmp/solar report.html"));
        assert!(
            command.contains("\"/tmp/solar report.html\""),
            "a path with a space must be quoted: {command}"
        );
        // A plain path is left alone rather than needlessly quoted.
        assert!(!open_command(std::path::Path::new("/tmp/plain.html")).contains('"'));
    }
}
