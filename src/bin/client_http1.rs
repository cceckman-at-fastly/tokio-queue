//! Send N requests to the local server, on multiple HTTP/1 connections.

use std::{sync::Arc, time::Instant};

use bytes::Bytes;
use http::Request;
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use tokio::{net::TcpStream, runtime, sync::Barrier, task::JoinSet};

fn main() {
    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut requests = JoinSet::new();
        let barrier = Arc::new(Barrier::new(5));
        for i in 0..5 {
            let barrier = barrier.clone();
            requests.spawn(async move {
                let mut sender = if i == 0 {
                    let conn = TokioIo::new(TcpStream::connect("localhost:3000").await.unwrap());
                    let (mut sender, conn) =
                        hyper::client::conn::http1::handshake::<_, Full<Bytes>>(conn)
                            .await
                            .unwrap();
                    tokio::spawn(conn);

                    // Mimic the browser: pre-warm this connection before attempting any other fetches.
                    let uri = format!("http://localhost:3000/")
                        .parse()
                        .expect("could not parse URI");
                    let mut req = Request::new(Full::default());
                    *req.uri_mut() = uri;
                    println!("starting prewarm request...");
                    sender.send_request(req).await.unwrap();
                    println!("prewarm done");

                    // Wait for all the other tasks to be ready.
                    barrier.wait().await;
                    sender
                } else {
                    // Wait for the pre-warming request to complete before we start.
                    barrier.wait().await;
                    let conn = TokioIo::new(TcpStream::connect("localhost:3000").await.unwrap());
                    let (sender, conn) =
                        hyper::client::conn::http1::handshake::<_, Full<Bytes>>(conn)
                            .await
                            .unwrap();
                    tokio::spawn(conn);
                    sender
                };

                let uri = format!("http://localhost:3000/{i}/img.png")
                    .parse()
                    .expect("could not parse URI");
                let mut req = Request::new(Full::default());
                *req.uri_mut() = uri;
                let start = Instant::now();
                let _ = sender.send_request(req).await.unwrap();
                let end = Instant::now();
                end.duration_since(start)
            });
        }
        // And wait for all the requests to complete
        let times = requests.join_all().await;

        for (i, duration) in times.into_iter().enumerate() {
            println!("query {i}: {}", duration.as_millis());
        }
    });
}
