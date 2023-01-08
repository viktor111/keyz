mod server;

#[tokio::main]
async fn main() {
    let socket_addr = server::helpers::socket_address_from_string_ip("127.0.0.1:7667".to_string()).expect("Invalid IP address");
    let listener = server::helpers::create_listener(socket_addr).await.unwrap();

    server::init::start(&listener).await;
}
