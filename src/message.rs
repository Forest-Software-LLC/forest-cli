use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

/// Shared spinner frames so every live display (main spinner, install
/// progress bars) animates identically. Last frame doubles as indicatif's
/// finished state; all our bars finish_and_clear, so it never shows.
pub const TICK_STRINGS: &[&str] = &["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];

/// Appended to registry 404s, which also mean "not shared with you".
pub const PRIVATE_404_HINT: &str = "If this is a private package, you will need to be authorized by the package maintainer.";

/// A simple spinner-based message utility, similar to Ora in JS.
pub struct Message {
    spinner: ProgressBar,
    message: String,
}

pub enum MessageType {
    Success,
    Fail,
    Info,
    Warn,
}

pub fn success(text: &str) {
    println!("{} {}", "🌳".green(), text.green());
}

/// A failure mid-command. Printed as an error block: see `error_layout`.
pub fn fail(text: &str) {
    let width = terminal_width(&dialoguer::console::Term::stdout());
    for line in error_lines(text, &[], width) {
        println!("{}", line);
    }
}

/// The error a command ends with, and its causes, on stderr.
pub fn error(err: &anyhow::Error) {
    let mut causes: Vec<String> = Vec::new();
    for cause in err.chain().skip(1).map(|c| c.to_string()) {
        if !causes.contains(&cause) {
            causes.push(cause);
        }
    }
    error_block(&err.to_string(), &causes);
}

/// A headline plus detail paragraphs (one per cause, or one per line of a
/// report), on stderr.
pub fn error_block(headline: &str, details: &[String]) {
    let width = terminal_width(&dialoguer::console::Term::stderr());
    for line in error_lines(headline, details, width) {
        eprintln!("{}", line);
    }
}

/// Columns before an error's detail lines: the width of "🥀 ", so details
/// start under the headline text.
const DETAIL_INDENT: &str = "   ";

/// Errors wrap here even on a wide terminal, where long lines get hard to
/// follow.
const MAX_ERROR_WIDTH: usize = 100;

/// One column short of the terminal: a line that fills the last column
/// leaves a blank line behind it in some terminals.
fn terminal_width(term: &dialoguer::console::Term) -> Option<usize> {
    term.size_checked().map(|(_, cols)| usize::from(cols).saturating_sub(1).min(MAX_ERROR_WIDTH))
}

fn error_lines(text: &str, details: &[String], width: Option<usize>) -> Vec<String> {
    let (headline, body) = error_layout(text, details, width);
    let mut lines: Vec<String> = headline
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let lead = if i == 0 { format!("{} ", "🥀".red()) } else { DETAIL_INDENT.to_string() };
            format!("{}{}", lead, line.red().bold())
        })
        .collect();
    lines.extend(body);
    lines
}

/// Lay out an error for reading. Only the headline is bold red: the first
/// sentence of `text`, which says what failed. Its other sentences, then
/// each of `details`, follow as plain paragraphs indented under the
/// headline and word-wrapped to `width` (unwrapped when that's unknown, as
/// when piped to a log). A long headline wraps under itself too. Returns
/// the headline's lines and the body's, uncolored so tests can read them.
fn error_layout(text: &str, details: &[String], width: Option<usize>) -> (Vec<String>, Vec<String>) {
    let text = text.trim();
    let (first_line, more_lines) = text.split_once('\n').unwrap_or((text, ""));
    let (headline, rest) = split_first_sentence(first_line.trim());
    let paragraphs = rest
        .into_iter()
        .chain(more_lines.lines())
        .chain(details.iter().map(String::as_str))
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let wrap_at = width.map(|w| w.saturating_sub(DETAIL_INDENT.len()).max(20));
    let mut body = Vec::new();
    for paragraph in paragraphs {
        for line in wrap(paragraph, wrap_at) {
            body.push(format!("{}{}", DETAIL_INDENT, line));
        }
    }
    (wrap(headline, wrap_at), body)
}

/// Split after the first sentence: a ". " followed by a capital letter, so
/// "e.g. src/init.luau", versions, and file names stay whole.
fn split_first_sentence(text: &str) -> (&str, Option<&str>) {
    for (i, _) in text.match_indices(". ") {
        let rest = &text[i + 2..];
        if rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            return (&text[..=i], Some(rest));
        }
    }
    (text, None)
}

fn wrap(text: &str, width: Option<usize>) -> Vec<String> {
    let Some(width) = width else {
        return vec![text.to_string()];
    };
    let measure = dialoguer::console::measure_text_width;
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && measure(&line) + 1 + measure(word) > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

pub fn warn(text: &str) {
    println!("{} {}", "⚠️ ".yellow(), text.yellow());
}

pub fn info(text: &str) {
    // Trailing space matches warn(): ℹ️/⚠️ are text symbols + VS16 that many
    // terminals draw two cells wide while only advancing the cursor one.
    println!("{} {}", "ℹ️ ".cyan(), text.cyan());
}

impl Message {
    /// Create and start a new spinner with the given message.
    pub fn new(message: &str) -> Self {
        let spinner = ProgressBar::new_spinner();
        let style = ProgressStyle::with_template("{spinner:.green} {msg}")
            .unwrap()
            //.tick_strings(&["▂","▃","▅","▆","▇","█","▓","▒","░"," "]);
            .tick_strings(TICK_STRINGS);
        spinner.set_style(style);
        spinner.enable_steady_tick(Duration::from_millis(70));
        spinner.set_message(message.to_string());
        Message {
            spinner,
            message: message.to_string(),
        }
    }

    pub fn destroy(self) {
        self.spinner.finish_and_clear();
    }

    /// Update the spinner's message text.
    pub fn update(&mut self, message: &str) {
        self.message = message.to_string();
        self.spinner.set_message(message.to_string());
    }

    /// Hide the spinner while something else (e.g. download progress bars)
    /// owns the terminal; two live draw systems fight over the cursor and
    /// leave orphaned spinner lines behind. Pair with `resume`.
    pub fn pause(&self) {
        self.spinner.finish_and_clear();
    }

    /// Restart the spinner after `pause`, keeping the latest message.
    pub fn resume(&mut self) {
        *self = Message::new(&self.message);
    }

    /// Emit a styled final message, then restart the spinner.
    pub fn emit(&mut self, mtype: MessageType, text: &str) {
        self.finish(mtype, text);
        // restart spinner with original message
        *self = Message::new(&self.message);
    }

    pub fn finish(&self, mtype: MessageType, text: &str ) {
        self.spinner.finish_and_clear();
        match mtype {
            MessageType::Success => success(text),
            MessageType::Fail => fail(text),
            MessageType::Info => info(text),
            MessageType::Warn => warn(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_headline_is_the_first_sentence_and_the_rest_is_plain_detail() {
        let (headline, body) = error_layout(
            "Publishing cancelled. Please add a README.md and try again.",
            &[],
            None,
        );
        assert_eq!(headline, vec!["Publishing cancelled."]);
        assert_eq!(body, vec!["   Please add a README.md and try again."]);

        // Abbreviations, versions and file names don't end a sentence.
        let (headline, body) = error_layout(
            "Set \"root\" in forest.json to its path, e.g. \"src/init.luau\".",
            &[],
            None,
        );
        assert_eq!(headline, vec!["Set \"root\" in forest.json to its path, e.g. \"src/init.luau\"."]);
        assert!(body.is_empty());
    }

    #[test]
    fn error_causes_and_extra_lines_become_indented_paragraphs() {
        let (headline, body) = error_layout(
            "Failed to fetch package info for a/b: HTTP 404\n  If this is private, ask for access.",
            &["Couldn't reach the registry.".to_string()],
            None,
        );
        assert_eq!(headline, vec!["Failed to fetch package info for a/b: HTTP 404"]);
        assert_eq!(body, vec!["   If this is private, ask for access.", "   Couldn't reach the registry."]);
    }

    #[test]
    fn error_details_wrap_under_the_headline() {
        let detail = "src/server/ holds files forest didn't install (Main.server.luau, Services). Installing replaces everything in a dependency folder.".to_string();
        let (_, body) = error_layout("Failed to install mount src/server", &[detail], Some(40));
        assert!(body.len() > 1, "{:?}", body);
        for line in &body {
            assert!(line.starts_with(DETAIL_INDENT), "{:?}", line);
            assert!(dialoguer::console::measure_text_width(line) <= 40, "{:?}", line);
        }
        // A long headline wraps at the same width, never mid-word.
        let (headline, _) = error_layout("Failed to fetch package information for nobody/does-not-exist: HTTP 404", &[], Some(40));
        assert_eq!(headline, vec!["Failed to fetch package information", "for nobody/does-not-exist: HTTP 404"]);

        // A word longer than the width gets a line of its own, unbroken.
        let (_, body) = error_layout("x", &["https://registry.forest.dev/a/very/long/path/that/cannot/break ok".to_string()], Some(30));
        assert_eq!(body[0], "   https://registry.forest.dev/a/very/long/path/that/cannot/break");
        assert_eq!(body[1], "   ok");
    }
}
