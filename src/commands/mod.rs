pub mod login;
pub mod logout;
pub mod whoami;
pub mod install;
pub mod initialize;
pub mod publish;
pub mod remove;
pub mod update;

pub use login::login_command;
pub use logout::logout_command;
pub use whoami::whoami_command;
pub use install::install_command;
pub use initialize::init_command;
pub use publish::publish_command;
pub use remove::remove_command;
pub use update::{update_command, maybe_notify_update};
// …etc