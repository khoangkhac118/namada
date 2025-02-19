use color_eyre::eyre::Result;
use namada::tendermint_rpc::HttpClient;
use namada_apps::cli::api::CliApi;
use namada_apps::{cli, logging};
use tracing_subscriber::filter::LevelFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // init error reporting
    color_eyre::install()?;

    // init logging
    logging::init_from_env_or(LevelFilter::INFO)?;

    let cmd = cli::namada_relayer_cli()?;
    // run the CLI
    CliApi::<()>::handle_relayer_command::<HttpClient>(None, cmd).await
}
