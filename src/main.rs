mod config;
mod server;

use std::sync::Arc;

use config::Config;
use server::error::Result;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("Server terminated: {err}");
    }
}

async fn run() -> Result<()> {
    let config = Config::load::<&str>(None)?;
    let socket_addr = config.server.socket_addr()?;
    let listener = server::helpers::create_listener(socket_addr).await?;

    let store = server::store::Store::with_config(config.store.clone());
    let protocol = Arc::new(config.protocol.clone());

    server::init::start(&listener, store, protocol).await;
    Ok(())
}
