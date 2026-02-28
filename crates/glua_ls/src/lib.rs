pub mod cmd_args;
mod context;
mod handlers;
mod logger;
mod meta_text;
mod server;
mod util;

pub use clap::Parser;
pub use cmd_args::*;
pub use server::{AsyncConnection, ExitError, run_ls};

#[macro_use]
extern crate rust_i18n;
rust_i18n::i18n!("./locales", fallback = "en");
