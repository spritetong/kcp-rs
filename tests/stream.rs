#[cfg(feature = "stream")]
#[tokio::test]
async fn test_stream() {
    use ::bytes::{Bytes, BytesMut};
    use ::futures::{SinkExt, StreamExt};
    use ::kcp::*;
    use ::kcp::transport::*;
    use ::log::info;
    use ::std::{sync::Arc, time::Duration};
    use ::tokio::select;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
        .format_timestamp_micros()
        .is_test(true)
        .try_init()
        .ok();

    #[cfg(feature = "udp")]
    let (s1, s2) = {
        use std::net::SocketAddr;
        use tokio::net::UdpSocket;

        let addr1: SocketAddr = "127.0.0.1:4321".parse().unwrap();
        let addr2: SocketAddr = "127.0.0.1:4322".parse().unwrap();

        // // Split Stream + Sink object into separate sink and stream.
        // let (sink, stream) =
        //     UdpFramed::new(UdpSocket::bind(addr1).await.unwrap(), BytesCodec::new()).split();
        // let stream1 = stream.filter_map(|x| ready(x.ok().map(|(x, _)| x)));
        // let sink1 = sink.with(move |x: Bytes| ready(io::Result::Ok((x, addr2))));

        // // Create stream and sink independently.
        // let udp2 = Arc::new(UdpSocket::bind(addr2).await.unwrap());
        // let stream2 = UdpFramed::new(udp2.clone(), BytesCodec::new())
        //     .filter_map(|x| ready(x.ok().map(|(x, _)| x)));
        // let sink2 = UdpFramed::new(udp2, BytesCodec::new())
        //     .with(move |x: Bytes| ready(io::Result::Ok((x, addr1))));

        (
            UdpTransport::new(UdpSocket::bind(addr1).await.unwrap(), addr2),
            UdpTransport::new(UdpSocket::bind(addr2).await.unwrap(), addr1),
        )
    };
    #[cfg(not(feature = "udp"))]
    let (s1, s2) = {
        let (tx1, rx2) = tokio::sync::mpsc::channel::<BytesMut>(1024);
        let (tx2, rx1) = tokio::sync::mpsc::channel::<BytesMut>(1024);
        (MpscTransport::new(tx1, rx1), MpscTransport::new(tx2, rx2))
    };

    let config = Arc::new(KcpConfig {
        nodelay: KcpNoDelayConfig::fastest(),
        session_key: Bytes::copy_from_slice(&rand::random::<[u8; 16]>()),
        ..Default::default()
    });

    let (s1, s2) = tokio::join!(
        KcpStream::accept::<_, BytesMut, _>(
            config.clone(),
            Kcp::rand_conv(),
            s1,
            futures::sink::drain(),
            None,
        ),
        KcpStream::connect::<_, BytesMut, _>(config, s2, futures::sink::drain(), None),
    );
    let mut s1 = Box::new(s1.unwrap());
    let mut s2 = Box::new(s2.unwrap());

    s1.send(Bytes::from_static(b"12345")).await.unwrap();
    //s1.shutdown_immediately();
    info!("{:?}", s2.next().await);

    //assert!(s1.next().await.is_none());
    //assert!(s2.next().await.is_none());

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

    s2.flush().await.unwrap();
    s1.close().await.unwrap();
    s2.close().await.unwrap();
    kcp_sys_shutdown().await;
}
