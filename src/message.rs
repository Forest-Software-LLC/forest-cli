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
    println!("{} {}", "🌳".green(), text.green().bold());
}

pub fn fail(text: &str) {
    println!("{} {}", "🥀".red(), text.red().bold());
}

pub fn warn(text: &str) {
    println!("{} {}", "⚠️ ".yellow(), text.yellow().bold());
}

pub fn info(text: &str) {
    println!("{} {}", "ℹ️".cyan(), text.cyan().bold());
}

impl Message {
    /// Create and start a new spinner with the given message.
    pub fn new(message: &str) -> Self {
        let spinner = ProgressBar::new_spinner();
        let style = ProgressStyle::with_template("{spinner:.green} {msg}")
            .unwrap()
            .tick_strings(&["▂","▃","▅","▆","▇","█","▓","▒","░"," "]);
            //.tick_strings(&["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]);
        spinner.set_style(style);
        spinner.enable_steady_tick(Duration::from_millis(70));
        spinner.set_message(message.to_string());
        Message {
            spinner,
            message: message.to_string(),
        }
    }

    /// Update the spinner's message text.
    pub fn update(&self, message: &str) {
        self.spinner.set_message(message.to_string());
    }

    /// Stop and clear the spinner.
    #[allow(dead_code)]
    pub fn stop(&self) {
        self.spinner.finish_and_clear();
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
