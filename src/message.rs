use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

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

pub fn fail(text: &str) {
    println!("{} {}", "🥀".red(), text.red().bold());
}

pub fn warn(text: &str) {
    println!("{} {}", "⚠️ ".yellow(), text.yellow().bold());
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
            .tick_strings(&["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]);
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
    /// owns the terminal — two live draw systems fight over the cursor and
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
