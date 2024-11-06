use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{event, Level};

use crate::{cli::Config, sockets::InterfaceSocket};
use crate::{BROADCAST_MDNS, PACKET_SIZE};

pub(crate) async fn reflect(
    server_socket: UdpSocket,
    sockets: Vec<InterfaceSocket>,
    config: Arc<Config>,
    cancellation_token: CancellationToken,
) {
    let mut buffer = vec![0u8; PACKET_SIZE];

    loop {
        let (recvsize, from_addr) = tokio::select! {
            () = cancellation_token.cancelled() => { break; },
            result = server_socket.recv_from(&mut buffer) => {
                match result {
                    Ok(ok) => ok,
                    Err(err) => {
                        event!(Level::ERROR, ?err, "recv()");
                        continue;
                    }
                }
            }
        };

        let is_loopback = sockets
            .iter()
            .any(|socket| socket.address == from_addr.ip());

        if is_loopback {
            continue;
        }

        event!(Level::INFO, "data from={} size={}", from_addr, recvsize);

        for socket in &sockets {
            // do not repeat packet back to the same network from which it originated
            if let SocketAddr::V4(socket_address) = from_addr {
                if socket_address.ip() & socket.mask == socket.network {
                    continue;
                }
            } else {
                event!(Level::INFO, "Got message from IPv6?");
            }

            if config.verbose {
                event!(Level::INFO, "repeating data to {}", socket.name);
            }

            // repeat data
            match socket
                .socket
                .send_to(&buffer[0..recvsize], &BROADCAST_MDNS)
                .await
            {
                Ok(sentsize) => {
                    if sentsize != recvsize {
                        event!(
                            Level::ERROR,
                            "send_packet size differs: sent={} actual={}",
                            recvsize,
                            sentsize
                        );
                    }
                },
                Err(err) => {
                    event!(Level::ERROR, ?err, "send()");
                },
            }
        }
    }

    // server_socket and sockets are dropped here
}
