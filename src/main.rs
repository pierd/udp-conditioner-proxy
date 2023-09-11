use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use clap::Parser;
use rand::Rng;
use tokio::net::UdpSocket;

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

// returns upstream sender
async fn spawn_upstream_connection(
    proxy_bind_addr: SocketAddr,
    upstream: SocketAddr,
    downstream: SocketAddr,
    downstream_tx: flume::Sender<DownstreamPacket>,
    delay_up: Duration,
    max_jitter: Duration,
    drop_probability: f64,
) -> flume::Sender<UpstreamPacket> {
    let sock = Arc::new(UdpSocket::bind(proxy_bind_addr).await.unwrap());
    let (upstream_tx, upstream_rx) = flume::unbounded::<UpstreamPacket>();

    // spawn upstream receiver
    tokio::spawn({
        let sock = sock.clone();
        async move {
            let mut buf = [0; BUFFER_SIZE];
            while let Ok((len, from_addr)) = sock.recv_from(&mut buf).await {
                if from_addr != upstream {
                    eprintln!(
                        "received data from unexpected address: {} (expected: {})",
                        from_addr, upstream
                    );
                    continue;
                }
                if rand::thread_rng().gen_bool(drop_probability) {
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
            tokio::time::sleep_until((recv_time + delay_up + random_duration(max_jitter)).into())
                .await;
            if sock.send_to(&data[..len], upstream).await.is_err() {
                eprintln!("failed to send to {}", upstream);
                break;
            }
        }
    });

    upstream_tx
}

fn random_duration(max_duration: Duration) -> Duration {
    let max_micros = max_duration.as_micros() as u64;
    if max_micros == 0 {
        return Duration::from_micros(0);
    }
    let micros = rand::thread_rng().gen_range(0..max_micros);
    Duration::from_micros(micros)
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Change the bind address
    #[arg(
        short,
        long,
        value_name = "BIND_ADDRESS",
        default_value = "127.0.0.1:7777"
    )]
    from: SocketAddr,

    /// Upstream address (where to proxy to)
    #[arg(short, long, value_name = "ADDRESS", default_value = "127.0.0.1:9000")]
    to: SocketAddr,

    /// Delay to add to each packet
    #[arg(short, long, value_name = "MILLIS", default_value = "0")]
    delay: u64,

    /// Delay to add to each packet going from client to server (upstream)
    #[arg(long, value_name = "MILLIS", default_value = "0")]
    delay_up: u64,

    /// Delay to add to each packet going from server to client (downstream)
    #[arg(long, value_name = "MILLIS", default_value = "0")]
    delay_down: u64,

    /// Jitter to add to each packet
    #[arg(long, value_name = "MILLIS", default_value = "0")]
    jitter: u64,

    /// Loss rate
    #[arg(short, long, value_name = "DROP_PROBABILITY", default_value = "0")]
    loss: f64,
}

#[tokio::main]
async fn main() {
    // parse the command line arguments
    let args = Cli::parse();
    let bind_addr = args.from;
    let proxy_bind_addr = SocketAddr::new(bind_addr.ip(), 0); // any port on the same ip
    let upstream = args.to;
    let delay_up = Duration::from_millis(args.delay) + Duration::from_millis(args.delay_up);
    let delay_down = Duration::from_millis(args.delay) + Duration::from_millis(args.delay_down);
    let max_jitter = Duration::from_millis(args.jitter);
    let drop_probability = args.loss;

    // channel for sending packets to downstream
    let (downstream_tx, downstream_rx) = flume::unbounded::<DownstreamPacket>();

    // spawn downstream receiver
    let sock = Arc::new(UdpSocket::bind(bind_addr).await.unwrap());
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
                        eprintln!("-> {} <-> {}", from_addr, upstream);
                        let upstream_tx = spawn_upstream_connection(
                            proxy_bind_addr,
                            upstream,
                            from_addr,
                            downstream_tx.clone(),
                            delay_up,
                            max_jitter,
                            drop_probability,
                        )
                        .await;
                        upstream_senders.insert(from_addr, upstream_tx.clone());
                        upstream_tx
                    }
                };

                if rand::thread_rng().gen_bool(drop_probability) {
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
                tokio::time::sleep_until(
                    (recv_time + delay_down + random_duration(max_jitter)).into(),
                )
                .await;
                if sock.send_to(&data[..len], target).await.is_err() {
                    eprintln!("failed to send to {}", target);
                    break;
                }
            }
        }
    });

    // wait for ctrl-c
    tokio::signal::ctrl_c().await.unwrap();
}
