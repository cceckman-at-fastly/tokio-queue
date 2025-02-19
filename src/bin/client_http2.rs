//! Send N requests to the local server, on a single HTTP/2 connection.

use std::time::Instant;

use bytes::Bytes;
use http::Request;
use http_body_util::Full;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::{net::TcpStream, runtime, task::JoinSet};

fn main() {
    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let conn = TokioIo::new(TcpStream::connect("localhost:3000").await.unwrap());
        // Create the Hyper client
        let (sender, conn) = hyper::client::conn::http2::handshake::<_, _, Full<Bytes>>(
            TokioExecutor::default(),
            conn,
        )
        .await
        .unwrap();
        let runner = rt.spawn(conn);

        let mut js = JoinSet::new();
        for i in 0..5 {
            let uri = format!("http://localhost:3000/{i}/img.png")
                .parse()
                .expect("could not parse URI");
            let mut req = Request::new(Full::default());
            *req.uri_mut() = uri;
            let mut sender = sender.clone();
            js.spawn(async move {
                let start = Instant::now();
                let _ = sender.send_request(req).await;
                let end = Instant::now();
                end.duration_since(start)
            });
        }
        std::mem::drop(sender);
        let times = js.join_all().await;
        for (i, duration) in times.into_iter().enumerate() {
            println!("query {i}: {}", duration.as_millis());
        }

        let _ = runner.await;
    });
}
