use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicI8, Ordering::SeqCst},
        Arc,
    },
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::Path,
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use image::{ImageFormat, Pixel, RgbImage};
use rand::random;
use tokio::runtime;

/// A small tracker for how many threads are running/parked.
#[derive(Default)]
struct ThreadTracker {
    started: AtomicI8,
    parked: AtomicI8,
}

impl ThreadTracker {
    fn on_thread_start(self: &Arc<Self>) -> impl Fn() {
        let t = self.clone();
        move || {
            let v = t.started.fetch_add(1, SeqCst);
            tracing::info!("started: {} started", v + 1);
        }
    }
    fn on_thread_stop(self: &Arc<Self>) -> impl Fn() {
        let t = self.clone();
        move || {
            let v = t.started.fetch_add(-1, SeqCst);
            tracing::info!("stopped: {} started", v - 1);
        }
    }
    fn on_thread_park(self: &Arc<Self>) -> impl Fn() {
        let t = self.clone();
        move || {
            let v = t.parked.fetch_add(1, SeqCst);
            tracing::info!("  parked: {} parked", v + 1);
        }
    }
    fn on_thread_unpark(self: &Arc<Self>) -> impl Fn() {
        let t = self.clone();
        move || {
            let v = t.parked.fetch_add(-1, SeqCst);
            tracing::info!("unparked: {} parked", v - 1);
        }
    }
}

fn main() {
    tracing_subscriber::fmt::init();

    let track = Arc::new(ThreadTracker::default());

    let rt = runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .on_thread_stop(track.on_thread_stop())
        .on_thread_start(track.on_thread_start())
        .on_thread_park(track.on_thread_park())
        .on_thread_unpark(track.on_thread_unpark())
        .build()
        .expect("could not create tokio runtime");

    rt.block_on(async {
        let app = Router::new()
            .route("/", get(home))
            .route("/{id}/img.png", get(image));
        let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
}

async fn home() -> impl IntoResponse {
    maud::html!(
        html {
            body {
            img src="1/img.png";
            img src="2/img.png";
            img src="3/img.png";
            img src="4/img.png";
            img src="5/img.png";
            }
        }
    )
}

#[axum::debug_handler]
async fn image(path: Path<String>) -> impl IntoResponse {
    let Path(path) = path;
    tracing::info!("starting image: {path}");
    let start = Instant::now();
    // Do some "CPU-bound" work...
    let mut img = RgbImage::new(32, 32);
    // Fill with a random color, so we can see it load.
    let rgb: [u8; 3] = [random(), random(), random()];
    img.pixels_mut()
        .for_each(|p| p.channels_mut().copy_from_slice(&rgb));
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .expect("could not write image");

    // ...for quite a while.
    let wait = Duration::from_secs(2) - Instant::now().duration_since(start);
    // Yes, we're doing a thread-level sleep in a future -- to simulate CPU-bound work.
    std::thread::sleep(wait);
    tracing::info!("finishing image response");
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "image/png")
        .body::<Body>(png.into())
        .expect("could not complete image response")
}
