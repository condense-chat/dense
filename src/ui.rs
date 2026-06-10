//! Tiny styling helpers over owo-colors — colors disable when stdout isn't a
//! terminal and honour `NO_COLOR` / `TERM=dumb`.

use owo_colors::{OwoColorize, Stream};

pub fn bold(s: &str) -> String {
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.bold()))
}

pub fn cyan(s: &str) -> String {
    format!(
        "{}",
        s.if_supports_color(Stream::Stdout, |t| t.cyan().bold().to_string())
    )
}

pub fn dim(s: &str) -> String {
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.dimmed()))
}

pub fn green(s: &str) -> String {
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.green()))
}

pub fn red(s: &str) -> String {
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.red()))
}

pub fn yellow(s: &str) -> String {
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.yellow()))
}
