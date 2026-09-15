//! The one HTTP call an API run makes, held to `--timeout` and interrupts the
//! same way the subprocess loop holds a child.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ureq::Agent;

use super::HttpRequest;
use crate::adapters::DriverError;
use crate::args::Options;
use crate::signals;

const POLL: Duration = Duration::from_millis(50);
/// ureq's own cap sits this far past dash-p's deadline, so the deadline decides
/// (a timeout, exit 20) rather than a transport error (exit 10). The cap only
/// makes sure the worker thread ends.
const TRANSPORT_GRACE: Duration = Duration::from_secs(5);
/// Cap for discovery calls (`list models`), which have no `--timeout`.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// What came back: a response with any status, or a transport failure (DNS,
/// connect, TLS) that never produced one.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Response { status: u16, body: String },
    Failed(String),
}

/// Send `request` on a worker thread and wait for it under the run's deadline.
/// A timeout or an interrupt returns at once; the worker is left to finish on
/// its own, the same as a stdout reader that a descendant holds open.
pub fn send(request: HttpRequest, opts: &Options) -> Result<Outcome, DriverError> {
    let start = Instant::now();
    let timeout = Duration::from_millis(opts.timeout_ms);
    let agent = agent(timeout + TRANSPORT_GRACE);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(perform(&agent, request));
    });
    loop {
        if signals::interrupted() {
            return Err(DriverError::Interrupted);
        }
        if start.elapsed() > timeout {
            return Err(DriverError::StopTimeout);
        }
        match rx.recv_timeout(POLL) {
            Ok(outcome) => return Ok(outcome),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(Outcome::Failed("the request thread stopped without a result".into()));
            }
        }
    }
}

/// A blocking GET for a discovery command. Any status outside 2xx is an error.
pub fn get(request: HttpRequest) -> Result<String, String> {
    match perform(&agent(LIST_TIMEOUT), request) {
        Outcome::Response { status, body } if (200..300).contains(&status) => Ok(body),
        Outcome::Response { status, body } => Err(format!("{status}: {}", super::excerpt(&body))),
        Outcome::Failed(why) => Err(why),
    }
}

/// An agent that returns every status as a response: the error body is what
/// says why a call failed, and a status-as-error would drop it.
fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

fn perform(agent: &Agent, request: HttpRequest) -> Outcome {
    let HttpRequest { url, headers, body } = request;
    let result = match body {
        Some(body) => headers
            .iter()
            .fold(agent.post(&url), |b, (name, value)| b.header(*name, value.as_str()))
            .send(body),
        None => headers
            .iter()
            .fold(agent.get(&url), |b, (name, value)| b.header(*name, value.as_str()))
            .call(),
    };
    match result {
        Ok(mut response) => {
            let status = response.status().as_u16();
            match response.body_mut().read_to_string() {
                Ok(body) => Outcome::Response { status, body },
                Err(e) => Outcome::Failed(e.to_string()),
            }
        }
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

#[cfg(test)]
pub mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// A one-shot HTTP server on a free local port. It answers one request with
    /// `status` and `body`, and sends the raw request it read back to the test.
    pub fn serve_once(status: u16, body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut payload = vec![0u8; length];
            let _ = reader.read_exact(&mut payload);
            let _ = tx.send(format!("{head}\r\n{}", String::from_utf8_lossy(&payload)));
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        (base, rx)
    }

    fn post(url: String) -> HttpRequest {
        HttpRequest { url, headers: vec![("x-test", "1".into())], body: Some("{}".into()) }
    }

    fn opts(timeout_ms: u64) -> Options {
        Options { timeout_ms, ..Options::default() }
    }

    #[test]
    fn a_response_of_any_status_comes_back_with_its_body() {
        let (base, request) = serve_once(429, r#"{"error":"slow down"}"#);
        let out = send(post(format!("{base}/v1/x")), &opts(5_000)).unwrap();
        assert_eq!(out, Outcome::Response { status: 429, body: r#"{"error":"slow down"}"#.into() });
        let raw = request.recv().unwrap();
        assert!(raw.starts_with("POST /v1/x "), "{raw}");
        assert!(raw.contains("x-test: 1"), "{raw}");
        assert!(raw.ends_with("{}"), "{raw}");
    }

    #[test]
    fn a_server_that_never_answers_is_held_to_the_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let started = Instant::now();
        let err = send(post(base), &opts(300)).err().unwrap();
        assert!(matches!(err, DriverError::StopTimeout), "got: {err}");
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(listener);
    }

    #[test]
    fn a_refused_connection_is_a_transport_failure() {
        let out = send(post("http://127.0.0.1:9/".into()), &opts(5_000)).unwrap();
        assert!(matches!(out, Outcome::Failed(_)), "{out:?}");
    }

    #[test]
    fn get_turns_a_non_2xx_status_into_an_error() {
        let (base, _) = serve_once(401, r#"{"error":"bad key"}"#);
        let request = HttpRequest { url: base, headers: vec![], body: None };
        let err = get(request).unwrap_err();
        assert!(err.starts_with("401: "), "{err}");
    }
}
