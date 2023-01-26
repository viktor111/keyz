use tokio::io::AsyncWriteExt;
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
            let command = match helpers::read_message(&mut stream).await {
                Ok(command) => command,
                Err(e) => {
                    println!("[-] Failed to read command: {}", e);
                    break;
                }
            };

            if command == "CLOSE" {
                let response = "Closing connection";
                println!("[.] Closing connection");
                match helpers::write_message(&mut stream, &response).await {
                    Ok(_) => (),
                    Err(e) => {
                        println!("[-] Failed to write response: {}", e);
                        break;
                    }
                }
                match stream.shutdown().await {
                    Ok(_) => (),
                    Err(e) => {
                        println!("[-] Failed to close connection closing by force: {}", e);
                        break;
                    }
                }

                break;
            }

            let response = match dispatcher(command, &mut store).await {
                Ok(response) => response,
                Err(e) => {
                    println!("[-] Failed to dispatch command: {}", e);
                    break;
                }
            };

            match helpers::write_message(&mut stream, &response)
                .await
            {
                Ok(_) => (),
                Err(e) => {
                    println!("[-] Failed to write response: {}", e);
                    break;
                }
            }
        }
    });
}
