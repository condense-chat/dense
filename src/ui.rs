//! Tiny styling helpers over owo-colors — colors disable when stdout isn't a
//! terminal and honour `NO_COLOR` / `TERM=dumb`.

use owo_colors::{OwoColorize, Stream};

// Landing palette: --cta, --ok, --accent.
pub fn blue(s: &str) -> String {
    format!(
        "{}",
        s.if_supports_color(Stream::Stdout, |t| t.truecolor(0x2f, 0x6f, 0xeb))
    )
}

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
    format!(
        "{}",
        s.if_supports_color(Stream::Stdout, |t| t.truecolor(0x1b, 0x8a, 0x3a))
    )
}

pub fn orange(s: &str) -> String {
    format!(
        "{}",
        s.if_supports_color(Stream::Stdout, |t| t.truecolor(0xe8, 0x87, 0x3a))
    )
}

pub fn red(s: &str) -> String {
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.red()))
}

pub fn yellow(s: &str) -> String {
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.yellow()))
}
