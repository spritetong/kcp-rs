#[cfg(feature = "udp")]
#[tokio::test]
async fn test_udp_stream() {
    use ::bytes::Bytes;
    use ::futures::{SinkExt, StreamExt};
    use ::kcp::udp::*;
    use ::log::info;
    use ::std::{sync::Arc, time::Duration};
    use ::tokio::select;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
        .format_timestamp_micros()
        .is_test(true)
        .try_init()
        .ok();
    kcp_initialize();

    let config = Arc::new(KcpConfig {
        nodelay: KcpNoDelayConfig::fastest(),
        session_key: Bytes::copy_from_slice(&rand::random::<[u8; 16]>()),
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

    for _ in 0..100 {
        let (x, y) = tokio::join!(
            KcpUdpStream::connect(config.clone(), server_addr),
            server.next(),
        );
        x.unwrap().0.close().await.ok();
        y.unwrap().0.close().await.ok();
    }

    let mut s1 = s1.unwrap().0;
    let mut s2 = s2.unwrap().0;

    s1.send(Bytes::from_static(b"12345")).await.unwrap();
    info!("{:?}", s2.next().await);

    let frame = Bytes::from(vec![0u8; 300000]);
    let start = std::time::Instant::now();
    let mut received = 0;
    let mut sent = 0;
    while start.elapsed() < Duration::from_secs(10) {
        select! {
            _ = s1.send(frame.clone()) => {
                sent += frame.len();
            },
            Some(Ok(x)) = s2.next() => {
                received += x.len();
            }
        }
    }
    while received < sent {
        match s2.next().await {
            Some(Ok(x)) => {
                received += x.len();
            }
            _ => break,
        }
    }
    info!("total sent {}, total received {}", sent, received);
    assert_eq!(sent, received);

    s1.close().await.unwrap();
    s2.close().await.unwrap();
    server.close().await.unwrap();
    kcp_cleanup().await;
}
