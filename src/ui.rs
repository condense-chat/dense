use std::fmt::Display;
use std::io::IsTerminal;
use std::time::Duration;

use console::{Style, Term};
use indicatif::{ProgressBar, ProgressStyle};

pub struct Bar<'a> {
    pub fraction: Option<f64>,
    pub label: &'a str,
    pub paint: fn(&str) -> String,
    pub value: String,
}

#[must_use]
pub struct Progress(Option<ProgressBar>);

pub struct Report {
    text: String,
    width: usize,
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(bar) = &self.0 {
            bar.finish_and_clear();
        }
    }
}

impl Report {
    pub fn bars(&mut self, rows: &[Bar<'_>]) {
        let value_width = rows
            .iter()
            .map(|row| console::measure_text_width(&row.value))
            .max()
            .unwrap_or(0);
        let inline = self.width >= 60 && self.width.saturating_sub(value_width + 20) >= 8;
        let cells = if inline {
            self.width.saturating_sub(value_width + 20).min(28)
        } else {
            self.width.saturating_sub(4).min(28)
        };
        for row in rows {
            let Some(fraction) = row.fraction.filter(|n| n.is_finite()) else {
                self.field(row.label, &row.value);
                continue;
            };
            let chart = bar(fraction, cells, row.paint);
            if inline {
                self.text
                    .push_str(&format!("  {:<14}  {chart}  {}\n", row.label, row.value));
            } else {
                self.field(row.label, &row.value);
                self.text.push_str(&format!("    {chart}\n"));
            }
        }
    }

    pub fn field(&mut self, label: &str, value: impl Display) {
        self.text
            .push_str(&field_at(label, &value.to_string(), self.width));
        self.text.push('\n');
    }

    pub fn finish(self) -> String {
        self.text
    }

    pub fn new(title: &str, width: usize) -> Self {
        let width = width.min(100);
        Self {
            text: format!(
                "\n{}\n",
                wrap_at(&format!("{} / {}", cyan("dense"), bold(title)), "  ", width)
            ),
            width,
        }
    }

    pub fn note(&mut self, text: &str) {
        self.text.push_str(&dim(&wrap_at(text, "  ", self.width)));
        self.text.push('\n');
    }

    pub fn section(&mut self, title: &str) {
        self.text.push('\n');
        self.text.push_str(&bold(&wrap_at(title, "  ", self.width)));
        self.text.push('\n');
    }
}

pub fn blue(s: &str) -> String {
    paint(s, Style::new().blue(), false)
}

pub fn bold(s: &str) -> String {
    paint(s, Style::new().bold(), false)
}

pub fn check(ok: bool, label: &str, detail: &str) {
    let (tag, style) = if ok {
        ("OK", Style::new().green())
    } else {
        ("CHECK", Style::new().yellow())
    };
    message(tag, label, style, false);
    if !detail.is_empty() {
        self::detail(detail);
    }
}

pub fn color_enabled(stderr: bool) -> bool {
    let terminal = if stderr {
        std::io::stderr().is_terminal()
    } else {
        std::io::stdout().is_terminal()
    };
    terminal
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var("TERM").ok().is_none_or(|v| v != "dumb")
}

pub fn cyan(s: &str) -> String {
    paint(s, Style::new().cyan().bold(), false)
}

pub fn detail(text: impl Display) {
    println!("{}", dim(&wrapped(&text.to_string(), "          ", false)));
}

pub fn dim(s: &str) -> String {
    paint(s, Style::new().dim(), false)
}

pub fn error(error: &(dyn std::error::Error + 'static)) {
    message("ERROR", &error.to_string(), Style::new().red(), true);
    let mut cause = error.source();
    while let Some(source) = cause {
        stderr_detail(format!("Caused by: {source}"));
        cause = source.source();
    }
}

pub fn field(label: &str, value: impl Display) {
    println!(
        "{}",
        field_at(label, &value.to_string(), width(false).min(100))
    );
}

pub fn green(s: &str) -> String {
    paint(s, Style::new().green(), false)
}

pub fn heading(title: &str) -> String {
    format!("\n  {} / {}\n", cyan("dense"), bold(title))
}

pub fn hint(label: &str, command: &str) {
    let label = dim(&format!("{label}:"));
    if console::measure_text_width(&label) + console::measure_text_width(command) + 3 > width(false)
    {
        println!("\n  {label}\n    {}", cyan(command));
    } else {
        println!("\n  {label} {}", cyan(command));
    }
}

pub fn init() {
    console::set_colors_enabled(color_enabled(false));
    console::set_colors_enabled_stderr(color_enabled(true));
}

pub fn interactive() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && std::env::var("TERM").ok().is_none_or(|v| v != "dumb")
}

pub fn note(text: impl Display) {
    message("INFO", &text.to_string(), Style::new().cyan(), false);
}

pub fn orange(s: &str) -> String {
    paint(s, Style::new().color256(208), false)
}

pub fn progress(message: &str) -> Progress {
    if !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
        || std::env::var("TERM").is_ok_and(|v| v == "dumb")
    {
        return Progress(None);
    }
    let template = if color_enabled(true) {
        "  {spinner:.cyan}  {msg}"
    } else {
        "  {spinner}  {msg}"
    };
    let Ok(style) = ProgressStyle::with_template(template) else {
        return Progress(None);
    };
    let bar = ProgressBar::new_spinner().with_style(style.tick_strings(&[
        "[==    ]", "[ ==   ]", "[  ==  ]", "[   == ]", "[    ==]", "[   == ]", "[  ==  ]",
        "[ ==   ]", "        ",
    ]));
    bar.set_message(
        console::truncate_str(message, width(true).saturating_sub(12), "...").into_owned(),
    );
    bar.tick();
    bar.enable_steady_tick(Duration::from_millis(100));
    Progress(Some(bar))
}

pub fn red(s: &str) -> String {
    paint(s, Style::new().red(), false)
}

pub fn section(title: &str) {
    println!("\n  {}", bold(title));
}

pub fn stderr_detail(text: impl Display) {
    eprintln!("{}", wrapped(&text.to_string(), "          ", true));
}

pub fn stderr_note(text: impl Display) {
    message("INFO", &text.to_string(), Style::new().cyan(), true);
}

pub fn stderr_title(title: &str) {
    eprintln!(
        "\n  {} / {}\n",
        paint("dense", Style::new().cyan().bold(), true),
        paint(title, Style::new().bold(), true)
    );
}

pub fn stderr_success(text: impl Display) {
    message("OK", &text.to_string(), Style::new().green(), true);
}

pub fn success(text: impl Display) {
    message("OK", &text.to_string(), Style::new().green(), false);
}

pub fn title(title: &str) {
    println!("{}", heading(title));
}

pub fn warning(text: impl Display) {
    message("WARN", &text.to_string(), Style::new().yellow(), true);
}

pub fn width(stderr: bool) -> usize {
    let term = if stderr {
        Term::stderr()
    } else {
        Term::stdout()
    };
    term.size_checked()
        .map(|(_, cols)| usize::from(cols))
        .filter(|&cols| cols > 0)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()?
                .parse()
                .ok()
                .filter(|&n| n > 0)
        })
        .unwrap_or(80)
}

pub fn yellow(s: &str) -> String {
    paint(s, Style::new().yellow(), false)
}

fn bar(fraction: f64, width: usize, paint: fn(&str) -> String) -> String {
    let fraction = fraction.clamp(0.0, 1.0);
    let units = ((fraction * width as f64 * 8.0).round() as usize)
        .max(usize::from(fraction > 0.0))
        .min(width * 8);
    let full = units / 8;
    let partial = units % 8;
    let mut filled = "█".repeat(full);
    if partial > 0 {
        filled.push([' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'][partial]);
    }
    let empty = width.saturating_sub(full + usize::from(partial > 0));
    format!("{}{}", paint(&filled), dim(&"░".repeat(empty)))
}

fn field_at(label: &str, value: &str, width: usize) -> String {
    if !label.is_empty() && width < 50 {
        format!("  {}\n{}", bold(label), wrap_at(value, "    ", width))
    } else {
        let prefix = if label.is_empty() {
            "  ".into()
        } else {
            format!("  {label:<14}  ")
        };
        wrap_at(value, &prefix, width)
    }
}

fn message(tag: &str, text: &str, style: Style, stderr: bool) {
    let prefix = format!("  {tag:<6}  ");
    let plain = wrapped(text, &prefix, stderr);
    let rendered = plain.replacen(tag, &paint(tag, style.bold(), stderr), 1);
    if stderr {
        eprintln!("{rendered}");
    } else {
        println!("{rendered}");
    }
}

fn paint(s: &str, style: Style, stderr: bool) -> String {
    style
        .force_styling(color_enabled(stderr))
        .apply_to(s)
        .to_string()
}

fn words(line: &str) -> Box<dyn Iterator<Item = textwrap::core::Word<'_>> + '_> {
    let mut quoted = false;
    Box::new(
        line.split_inclusive(move |c| {
            if c == '`' {
                quoted = !quoted;
            }
            c == ' ' && !quoted
        })
        .map(textwrap::core::Word::from),
    )
}

fn wrapped(text: &str, prefix: &str, stderr: bool) -> String {
    wrap_at(text, prefix, width(stderr).min(100))
}

fn wrap_at(text: &str, prefix: &str, width: usize) -> String {
    let indent = " ".repeat(console::measure_text_width(prefix));
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            textwrap::fill(
                line,
                textwrap::Options::new(width)
                    .initial_indent(if i == 0 { prefix } else { &indent })
                    .subsequent_indent(&indent)
                    .word_separator(textwrap::WordSeparator::Custom(words))
                    .word_splitter(textwrap::WordSplitter::NoHyphenation)
                    .break_words(false),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_wraps_and_explicit_lines_keep_their_indent() {
        let text = wrap_at(
            "A clear message for a narrow terminal.\nNext step.",
            "  INFO    ",
            32,
        );
        assert!(
            text.lines()
                .all(|line| console::measure_text_width(line) <= 32)
        );
        assert!(
            text.lines()
                .skip(1)
                .all(|line| line.starts_with("          "))
        );
        assert!(text.ends_with("          Next step."));
    }

    #[test]
    fn commands_stay_copyable_when_prose_wraps() {
        let text = wrap_at(
            "Log in with `dense login` to start using Condense.",
            "  INFO    ",
            32,
        );
        assert!(text.contains("`dense login`"));
        assert!(
            text.lines()
                .all(|line| console::measure_text_width(line) <= 32)
        );
    }

    #[test]
    fn long_paths_and_urls_are_not_split() {
        let path = "/a/long/path/that/needs/to/stay/copyable";
        assert!(wrap_at(path, "  File    ", 24).contains(path));
    }
}
