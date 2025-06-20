pub mod login;
pub mod install;
pub mod initialize;
pub mod publish;

pub use login::login_command;
pub use install::install_command;
pub use initialize::init_command;
pub use publish::publish_command;
// …etc