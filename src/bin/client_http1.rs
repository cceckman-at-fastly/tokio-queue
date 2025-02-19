//! Send N requests to the local server, on multiple HTTP/1 connections.

use std::time::Instant;

use bytes::Bytes;
use http::Request;
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use tokio::{net::TcpStream, runtime, task::JoinSet};

fn main() {
    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut requests = JoinSet::new();
        for i in 0..5 {
            let conn = TokioIo::new(TcpStream::connect("localhost:3000").await.unwrap());
            let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, Full<Bytes>>(conn)
                .await
                .unwrap();
            rt.spawn(conn);

            let uri = format!("http://localhost:3000/{i}/img.png")
                .parse()
                .expect("could not parse URI");
            let mut req = Request::new(Full::default());
            *req.uri_mut() = uri;
            requests.spawn(async move {
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
