//! Shared integration-test fixtures.
//!
//! Lets a test binary drive the real router against a scripted WDA. The
//! `AppState` itself comes from the single shared fixture
//! (`tests/fixtures/app_state.rs`), which `http_auth.rs` includes too — there
//! is one builder, not one per test binary.
//!
//! NOTE: like `http_auth.rs`, these use a hand-built current-thread runtime via
//! [`block`] rather than `#[tokio::test]` — the local crate named `core` sits in
//! this test crate's extern prelude and would shadow the std `core` that
//! `#[tokio::test]` expands to.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use server::core_crate::encode::{EncodedFrame, VideoPipeline};
use server::http::AppState;

/// Run a future to completion on a fresh current-thread runtime.
pub fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

/// A no-op pipeline: never emits frames; `request_keyframe` is a no-op.
struct NullPipeline {
    tx: tokio::sync::broadcast::Sender<EncodedFrame>,
}

impl VideoPipeline for NullPipeline {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EncodedFrame> {
        self.tx.subscribe()
    }
    fn request_keyframe(&self) {}
}

use server as srv;
use server::core_crate as srv_core;
include!("../fixtures/app_state.rs");

/// Shared fixture; see `crates/server/tests/fixtures/app_state.rs`.
pub fn build_state(password: Option<&str>) -> Arc<AppState> {
    fixture_app_state(password)
}

pub fn build_state_with_wda(base_url: &str) -> Arc<AppState> {
    let state = build_state(None);
    let mut state = match Arc::try_unwrap(state) {
        Ok(state) => state,
        Err(_) => panic!("test state unexpectedly shared"),
    };
    state.wda = Some(Arc::new(tokio::sync::Mutex::new(
        server::wda::WdaClient::new(base_url).unwrap(),
    )));
    Arc::new(state)
}

/// A scripted WDA that can be stopped.
///
/// The responder picks the body and an artificial delay per request; returning
/// `None` closes the connection without a response (ambiguous transport loss).
/// Unlike a detached mock thread, this one is bounded in every direction:
/// `accept` and `read` poll a stop flag, an artificial delay is interruptible,
/// and dropping the handle stops and JOINS the thread. A panic inside the
/// responder (a server-side assertion) is captured and re-raised on drop, so a
/// server-side failure fails the test instead of vanishing.
pub struct MockWda {
    url: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    failure: Arc<Mutex<Option<String>>>,
}

impl MockWda {
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for MockWda {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // The accept loop polls the flag, so no wake-up connection is
            // needed; joining is bounded by that poll interval.
            let _ = thread.join();
        }
        if let Some(failure) = self.failure.lock().unwrap().take() {
            if !std::thread::panicking() {
                panic!("scripted WDA failed: {failure}");
            }
        }
    }
}

/// Sleep `total`, waking every 25ms so a stopping mock never holds the test up
/// for the whole artificial delay.
fn interruptible_sleep(total: std::time::Duration, stop: &std::sync::atomic::AtomicBool) {
    let step = std::time::Duration::from_millis(25);
    let deadline = std::time::Instant::now() + total;
    while std::time::Instant::now() < deadline {
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        std::thread::sleep(std::cmp::min(
            step,
            deadline.saturating_duration_since(std::time::Instant::now()),
        ));
    }
}

pub fn mock_wda(
    responder: impl Fn(&str, usize) -> Option<(std::time::Duration, String)>
        + Send
        + std::panic::RefUnwindSafe
        + 'static,
) -> MockWda {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (thread_stop, thread_failure) = (stop.clone(), failure.clone());
    let thread = std::thread::spawn(move || {
        let mut index = 0_usize;
        while !thread_stop.load(std::sync::atomic::Ordering::Acquire) {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Err(_) => return,
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_millis(500)))
                .unwrap();
            let mut request = [0_u8; 8_192];
            let Ok(read) = stream.read(&mut request) else {
                continue;
            };
            let request = String::from_utf8_lossy(&request[..read]).to_string();
            let scripted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                responder(&request, index)
            }));
            index += 1;
            let scripted = match scripted {
                Ok(scripted) => scripted,
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "responder panicked".to_string());
                    *thread_failure.lock().unwrap() = Some(message);
                    return;
                }
            };
            let Some((delay, body)) = scripted else {
                continue;
            };
            interruptible_sleep(delay, &thread_stop);
            if thread_stop.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    MockWda {
        url: format!("http://{address}"),
        stop,
        thread: Some(thread),
        failure,
    }
}
