use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use rand::Rng;
use tokio::net::UdpSocket;

// address to bind to
const BIND: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7777));

// address to proxy to
const UPSTREAM: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9000));

// extra delay to add to each packet (practical minimum: 1ms)
const DELAY: std::time::Duration = std::time::Duration::from_millis(20);

// 5/100 packets will be dropped
const LOSS_RATE: (usize, usize) = (5, 100);

// TODO: make the above configurable via command line arguments

const ANY_LOCAL: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
const BUFFER_SIZE: usize = 4096;

// target = UPSTREAM
struct UpstreamPacket {
    data: [u8; BUFFER_SIZE],
    len: usize,
    recv_time: std::time::Instant,
}

// target = some downstream
struct DownstreamPacket {
    data: [u8; BUFFER_SIZE],
    len: usize,
    target: SocketAddr,
    recv_time: std::time::Instant,
}

fn should_drop() -> bool {
    let (num, denom) = LOSS_RATE;
    let mut rng = rand::thread_rng();
    rng.gen_range(0..denom) < num
}

// returns upstream sender
async fn spawn_upstream_connection(
    downstream: SocketAddr,
    downstream_tx: flume::Sender<DownstreamPacket>,
) -> flume::Sender<UpstreamPacket> {
    let sock = Arc::new(UdpSocket::bind(ANY_LOCAL).await.unwrap());
    let (upstream_tx, upstream_rx) = flume::unbounded::<UpstreamPacket>();

    // spawn upstream receiver
    tokio::spawn({
        let sock = sock.clone();
        async move {
            let mut buf = [0; BUFFER_SIZE];
            while let Ok((len, from_addr)) = sock.recv_from(&mut buf).await {
                if from_addr != UPSTREAM {
                    eprintln!(
                        "received data from unexpected address: {} (expected: {})",
                        from_addr, UPSTREAM
                    );
                    continue;
                }
                if should_drop() {
                    continue;
                }
                let recv_time = std::time::Instant::now();
                downstream_tx
                    .send_async(DownstreamPacket {
                        data: buf,
                        len,
                        target: downstream,
                        recv_time,
                    })
                    .await
                    .unwrap();
            }
        }
    });

    // spawn upstream sender
    tokio::spawn(async move {
        while let Ok(UpstreamPacket {
            data,
            len,
            recv_time,
        }) = upstream_rx.recv_async().await
        {
            tokio::time::sleep_until((recv_time + DELAY).into()).await;
            if sock.send_to(&data[..len], UPSTREAM).await.is_err() {
                eprintln!("failed to send to {}", UPSTREAM);
                break;
            }
        }
    });

    upstream_tx
}

#[tokio::main]
async fn main() {
    let sock = Arc::new(UdpSocket::bind(BIND).await.unwrap());
    let (downstream_tx, downstream_rx) = flume::unbounded::<DownstreamPacket>();

    // spawn downstream receiver
    tokio::spawn({
        let sock = sock.clone();
        let mut upstream_senders = HashMap::new();
        async move {
            let mut buf = [0; BUFFER_SIZE];
            while let Ok((len, from_addr)) = sock.recv_from(&mut buf).await {
                let recv_time = std::time::Instant::now();

                // get or spawn upstream sender
                let upstream_tx = match upstream_senders.get(&from_addr).cloned() {
                    Some(upstream_tx) => upstream_tx,
                    None => {
                        eprintln!("new connection from {}", from_addr);
                        let upstream_tx =
                            spawn_upstream_connection(from_addr, downstream_tx.clone()).await;
                        upstream_senders.insert(from_addr, upstream_tx.clone());
                        upstream_tx
                    }
                };

                if should_drop() {
                    continue;
                }

                upstream_tx
                    .send_async(UpstreamPacket {
                        data: buf,
                        len,
                        recv_time,
                    })
                    .await
                    .unwrap();
            }
        }
    });

    // spawn downstream sender
    tokio::spawn({
        let sock = sock.clone();
        async move {
            while let Ok(DownstreamPacket {
                data,
                len,
                target,
                recv_time,
            }) = downstream_rx.recv_async().await
            {
                tokio::time::sleep_until((recv_time + DELAY).into()).await;
                if sock.send_to(&data[..len], target).await.is_err() {
                    eprintln!("failed to send to {}", target);
                    break;
                }
            }
        }
    });

    tokio::signal::ctrl_c().await.unwrap();
}
