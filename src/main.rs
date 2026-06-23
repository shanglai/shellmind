mod platform;
mod registry;
mod embedder;
mod session;
mod disambiguator;
mod resolver;
mod model_client;
mod executor;
mod editor;
mod scheduler;
mod cli;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use platform::paths::ShellmindPaths;
use platform::shell::ShellEnv;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SHELLMIND_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn"))
        )
        .with_target(false)
        .init();

    let paths = ShellmindPaths::resolve()?;
    paths.ensure_dirs()?;

    let shell_env = ShellEnv::detect();
    let args: Vec<String> = std::env::args().collect();

    cli::dispatch(args, paths, shell_env).await
}
