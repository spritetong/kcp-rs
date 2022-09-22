use ::kcp::stream::*;

use ::bytes::Bytes;
use ::futures::{future::ready, SinkExt, StreamExt};
use ::log::error;
use ::std::{io, net::SocketAddr, sync::Arc, time::Duration};
use ::tokio::{net::UdpSocket, select};
use ::tokio_util::{codec::BytesCodec, udp::UdpFramed};

#[tokio::test]
async fn test_stream() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
        .format_timestamp_micros()
        .is_test(true)
        .try_init()
        .ok();

    let addr1: SocketAddr = "127.0.0.1:4321".parse().unwrap();
    let addr2: SocketAddr = "127.0.0.1:4322".parse().unwrap();

    let (sink, stream) =
        UdpFramed::new(UdpSocket::bind(addr1).await.unwrap(), BytesCodec::new()).split();
    let stream1 = stream.filter_map(|x| ready(x.ok().map(|(x, _)| x)));
    let sink1 = sink.with(move |x: Bytes| ready(io::Result::Ok((x, addr2))));

    let udp2 = Arc::new(UdpSocket::bind(addr2).await.unwrap());
    let stream2 = UdpFramed::new(udp2.clone(), BytesCodec::new())
        .filter_map(|x| ready(x.ok().map(|(x, _)| x)));
    let sink2 = UdpFramed::new(udp2, BytesCodec::new())
        .with(move |x: Bytes| ready(io::Result::Ok((x, addr1))));

    let config = Arc::new(KcpConfig {
        nodelay: KcpNoDelayConfig::normal(),
        session_key: rand::random(),
        ..Default::default()
    });

    let (s1, s2) = tokio::join!(
        KcpStream::accept(
            config.clone(),
            KcpStream::rand_conv(),
            sink1,
            stream1,
            futures::sink::drain(),
            None,
        ),
        KcpStream::connect(config, sink2, stream2, futures::sink::drain(), None),
    );
    let mut s1 = Box::new(s1.unwrap());
    let mut s2 = Box::new(s2.unwrap());

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

    s2.flush().await.unwrap();
    s1.close().await.unwrap();
    s2.close().await.unwrap();
}
