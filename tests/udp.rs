use ::kcp::udp::*;

use ::bytes::Bytes;
use ::futures::{SinkExt, StreamExt};
use ::log::error;
use ::std::{sync::Arc, time::Duration};
use ::tokio::select;

#[tokio::test]
async fn test_udp_stream() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
        .format_timestamp_micros()
        .is_test(true)
        .try_init()
        .ok();

    let config = Arc::new(KcpConfig {
        nodelay: KcpNoDelayConfig::normal(),
        session_key: rand::random(),
        ..Default::default()
    });

    let server_addr = "127.0.0.1:4323";
    let mut server = KcpUdpStream::listen(config.clone(), server_addr, 8)
        .await
        .unwrap();

    let (s1, s2) = tokio::join!(
        KcpUdpStream::connect(config.clone(), server_addr),
        server.next(),
    );

    for _ in 0..5 {
        //error!("start");
        let (x, y) = tokio::join!(
            KcpUdpStream::connect(config.clone(), server_addr),
            server.next(),
        );
        //error!("before close");
        x.unwrap().0.close().await.ok();
        y.unwrap().0.close().await.ok();
        //error!("after close");
    }

    let mut s1 = s1.unwrap().0;
    let mut s2 = s2.unwrap().0;

    s1.send(Bytes::from_static(b"12345")).await.unwrap();
    println!("{:?}", s2.next().await);

    let frame = Bytes::from(vec![0u8; 300000]);
    let start = std::time::Instant::now();
    let mut received = 0;
    while start.elapsed() < Duration::from_secs(10) {
        select! {
            _ = s1.send(frame.clone()) => (),
            Some(Ok(x)) = s2.next() => {
                //trace!("received {}", x.len());
                received += x.len();
            }
        }
    }
    error!("total received {}", received);

    s1.close().await.unwrap();
    s2.close().await.unwrap();
    server.close().await.unwrap();
}
