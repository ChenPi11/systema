mod ipc;
mod socket;

use anyhow::Result;
use tracing::info;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("system_k=debug".parse()?)
                .add_directive("common=debug".parse()?),
        )
        .init();

    libsysa::paths::init();

    info!("System K (Socket Worker) starting up");

    ipc::run().await?;

    Ok(())
}
