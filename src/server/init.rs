use tokio::net::{TcpListener, TcpStream};

use crate::server::dispatcher::dispatcher;
use crate::server::helpers;
use crate::server::store::Store;

pub async fn start(listener: &TcpListener) {
    loop {
        let conn = helpers::listener_accept_conn(&listener).await.unwrap();

        let stream = conn.0;

        handle_connection(stream).await;
    }
}

async fn handle_connection(mut stream: TcpStream) {
    let mut store = Store::new();

    tokio::spawn(async move {
        loop {
            let command = helpers::read_message(&mut stream)
                .await
                .expect("[-] Failed to read  command");

            let response = dispatcher(command, &mut store)
                .await
                .expect("[-] Failed to dispatch command");

            helpers::write_message(&mut stream, response)
                .await
                .expect("[-] Failed to write response");
        }
    });
}
