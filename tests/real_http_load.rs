//! Multi-virtual-user sustained load over real HTTP sockets.
//!
//! Drive multi-virtual-user *sustained* load through `Engine::run_all` using the
//! real `DefaultHttpClient` against a raw in-process HTTP/1.1 server over real
//! sockets.
//!
//! Prior integration coverage exercised either `Engine::run` single-pass
//! (config_cli_integration) or `run_all` with a *mock* client / a single VU
//! (metrics_integration). This test closes the remaining gap: many VUs, many passes
//! (a real time-boxed duration), real TCP, real reqwest — the exact loop the
//! crate exists to run — asserting the engine records real throughput and a
//! zero error rate against a healthy server.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use pummel::engine::{Engine, ExecutionOptions};
use pummel::scenario::{ScenarioBuilder, StepBuilder};

/// A minimal blocking HTTP/1.1 server on a detached background thread. It
/// answers every request with `200 OK` and `Connection: close`, spawning a
/// thread per connection so concurrent virtual users are served in parallel.
/// It lives for the whole test process.
struct TestServer {
    addr: SocketAddr,
    requests: Arc<AtomicUsize>,
}

impl TestServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let requests_bg = requests.clone();

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let requests = requests_bg.clone();
                thread::spawn(move || handle_conn(stream, &requests));
            }
        });

        TestServer { addr, requests }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn handle_conn(mut stream: TcpStream, requests: &AtomicUsize) {
    // Read the request head up to the blank line; bodies are irrelevant here.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65_536 {
                    break;
                }
            }
            Err(_) => return,
        }
    }

    requests.fetch_add(1, Ordering::SeqCst);

    let body = "ok";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_vu_sustained_load_real_http_client() {
    let server = TestServer::start();

    // Two-step DAG so each VU pass issues two real requests and dependency
    // ordering is exercised as well.
    let step1 = StepBuilder::new(
        "root",
        "Root",
        pummel::http::Request::get(format!("{}/", server.base()))
            .build()
            .unwrap(),
    )
    .max_retries(0)
    .timeout(Duration::from_secs(5))
    .build();

    let step2 = StepBuilder::new(
        "child",
        "Child",
        pummel::http::Request::get(format!("{}/child", server.base()))
            .build()
            .unwrap(),
    )
    .dependency("root")
    .max_retries(0)
    .timeout(Duration::from_secs(5))
    .build();

    // A real time-boxed run: 4 VUs generate load for 1s with a short ramp,
    // looping the DAG many times (closed loop, no think time).
    let scenario = ScenarioBuilder::new("sustained", "Sustained Load")
        .step(step1)
        .step(step2)
        .virtual_users(4)
        .duration(Duration::from_secs(1))
        .ramp_up(Duration::from_millis(50))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();

    let mut engine = Engine::new();
    engine.add_scenario(scenario);
    // NOTE: no with_http_client_factory override => the engine uses the real
    // DefaultHttpClient, so every request below crosses a real TCP socket.

    let options = ExecutionOptions::builder()
        .max_concurrent_requests(16)
        .build();

    let results = engine.run_all(options).await.unwrap();

    // Sustained load over real sockets produced many requests (far more than a
    // single single-pass would): 4 VUs x 2 steps looped for ~1s.
    assert!(
        results.total_requests >= 8,
        "sustained multi-VU load should record many requests, got {}",
        results.total_requests
    );
    assert_eq!(
        results.failed_requests, 0,
        "a healthy 200-only server must yield zero failures: {results:?}"
    );
    assert_eq!(
        results.error_rate, 0.0,
        "error rate must be zero against a healthy server: {results:?}"
    );
    assert_eq!(results.successful_requests, results.total_requests);
    assert_eq!(results.total_virtual_users, 4);

    // The engine's recorded request count must match what actually hit the
    // server socket (no double-counting, no phantom requests).
    assert_eq!(
        results.total_requests as usize,
        server.count(),
        "engine-recorded requests must equal requests the server actually served"
    );

    // Per-step aggregation is populated and consistent.
    let scen = results.scenarios.get("sustained").unwrap();
    let root = scen.steps.get("root").unwrap();
    let child = scen.steps.get("child").unwrap();
    assert!(root.total_requests > 0 && child.total_requests > 0);
    assert_eq!(root.failed_requests, 0);
    assert_eq!(child.failed_requests, 0);
}
