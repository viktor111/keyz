mod server;

use server::error::Result;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("Server terminated: {err}");
    }
}

async fn run() -> Result<()> {
    let socket_addr =
        server::helpers::socket_address_from_string_ip("127.0.0.1:7667".to_string())?;
    let listener = server::helpers::create_listener(socket_addr).await?;

    server::init::start(&listener).await;
    Ok(())
}
