pub mod login;
pub mod install;
pub mod initialize;
pub mod publish;
pub mod remove;

pub use login::login_command;
pub use install::install_command;
pub use initialize::init_command;
pub use publish::publish_command;
pub use remove::remove_command;
// …etc