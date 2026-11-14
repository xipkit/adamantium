//! A JSON-over-HTTPS client with bounded retries, shared by the providers.

use std::{
    io::ErrorKind,
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, bail};
use serde_json::Value;

const MAX_ATTEMPTS: usize = 4;
const MAX_SERVER_DELAY: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// A provider's HTTP endpoint and how patiently to call it.
pub(crate) struct JsonService {
    /// The name used in errors and retry notices, like "Decisions".
    pub name: &'static str,
    pub url: String,
    /// Headers sent with every request, including credentials.
    pub headers: Vec<(&'static str, String)>,
    /// The longest a single attempt may take.
    pub request_timeout: Duration,
    /// The longest all attempts for one request may take together.
    pub retry_budget: Duration,
    /// Whether an HTTP 429 body says the account is out of quota, so
    /// retrying cannot help.
    pub quota_exhausted: fn(&Value) -> bool,
    agent: ureq::Agent,
}

impl JsonService {
    /// A service that refuses plain HTTP and redirects.
    pub fn new(
        name: &'static str,
        url: &str,
        headers: Vec<(&'static str, String)>,
        request_timeout: Duration,
        retry_budget: Duration,
        quota_exhausted: fn(&Value) -> bool,
    ) -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(request_timeout))
            .build();
        Self::with_agent(
            config.into(),
            name,
            url,
            headers,
            request_timeout,
            retry_budget,
            quota_exhausted,
        )
    }

    /// A service that sends its requests through `agent`, for tests that
    /// stand in for the network.
    pub fn with_agent(
        agent: ureq::Agent,
        name: &'static str,
        url: &str,
        headers: Vec<(&'static str, String)>,
        request_timeout: Duration,
        retry_budget: Duration,
        quota_exhausted: fn(&Value) -> bool,
    ) -> Self {
        Self {
            name,
            url: url.to_owned(),
            headers,
            request_timeout,
            retry_budget,
            quota_exhausted,
            agent,
        }
    }

    /// Posts `request` and returns the JSON response, retrying transient
    /// failures with exponential backoff and jitter.
    #[allow(
        clippy::disallowed_methods,
        reason = "the CLI is synchronous and has no runtime to yield to; sleeping is the backoff"
    )]
    pub fn post(&self, request: &Value) -> Result<Value> {
        self.post_with(request, self.retry_budget, thread::sleep, || {
            Duration::from_millis(fastrand::u64(0..=250))
        })
    }

    pub(crate) fn post_with(
        &self,
        request: &Value,
        retry_budget: Duration,
        mut sleep: impl FnMut(Duration),
        mut jitter: impl FnMut() -> Duration,
    ) -> Result<Value> {
        let name = self.name;
        let started = Instant::now();
        for attempt in 1..=MAX_ATTEMPTS {
            let remaining = retry_budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                bail!("{name} request retry budget exhausted");
            }
            let failure = match self.request_once(request, remaining.min(self.request_timeout)) {
                Ok(response) => return Ok(response),
                Err(failure) => failure,
            };
            if !failure.retryable || attempt == MAX_ATTEMPTS {
                bail!("{} (after {attempt} attempt(s))", failure.error);
            }
            if failure
                .retry_after
                .is_some_and(|delay| delay > MAX_SERVER_DELAY)
            {
                bail!(
                    "{} (retry deferred: Retry-After exceeds {} seconds)",
                    failure.error,
                    MAX_SERVER_DELAY.as_secs()
                );
            }
            let backoff = Duration::from_secs(1 << (attempt - 1));
            let base = failure
                .retry_after
                .map_or(backoff, |delay| delay.max(backoff));
            let delay = base.saturating_add(jitter());
            if delay >= retry_budget.saturating_sub(started.elapsed()) {
                bail!("{} (retry budget exhausted)", failure.error);
            }
            eprintln!(
                "adamantium: temporary {name} failure; retrying after {:.2}s (attempt {}/{MAX_ATTEMPTS})",
                delay.as_secs_f64(),
                attempt + 1
            );
            sleep(delay);
        }
        bail!("{name} request failed after {MAX_ATTEMPTS} attempts")
    }

    fn request_once(
        &self,
        request: &Value,
        timeout: Duration,
    ) -> std::result::Result<Value, Failure> {
        let name = self.name;
        let mut builder = self.agent.post(&self.url);
        for (header, value) in &self.headers {
            builder = builder.header(*header, value);
        }
        let mut response = builder
            .header("Content-Type", "application/json")
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build()
            .send_json(request)
            .map_err(|error| Failure {
                retryable: transient_transport_error(&error),
                error: anyhow::anyhow!("{name} request failed: {error}"),
                retry_after: None,
            })?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| parse_retry_after(value, SystemTime::now()));
            let body = response
                .body_mut()
                .with_config()
                .limit(MAX_ERROR_BODY_BYTES)
                .read_json::<Value>()
                .ok();
            let quota_exhausted =
                status.as_u16() == 429 && body.as_ref().is_some_and(self.quota_exhausted);
            let message = body
                .as_ref()
                .and_then(|body| body.get("error")?.get("message")?.as_str())
                .map_or_else(String::new, |message| format!(": {message}"));
            return Err(Failure {
                error: anyhow::anyhow!("{name} API returned HTTP {status}{message}"),
                retryable: !quota_exhausted
                    && matches!(status.as_u16(), 408 | 409 | 429 | 500..=599),
                retry_after,
            });
        }
        let body = response.body_mut().read_to_vec().map_err(|error| Failure {
            retryable: transient_transport_error(&error),
            error: anyhow::anyhow!("failed to read {name} API response: {error}"),
            retry_after: None,
        })?;
        serde_json::from_slice(&body).map_err(|error| Failure {
            retryable: false,
            error: anyhow::anyhow!("invalid {name} API response: {error}"),
            retry_after: None,
        })
    }
}

struct Failure {
    error: anyhow::Error,
    retryable: bool,
    retry_after: Option<Duration>,
}

fn transient_transport_error(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Timeout(_) | ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
        ureq::Error::Io(error) => matches!(
            error.kind(),
            ErrorKind::TimedOut
                | ErrorKind::Interrupted
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::ConnectionRefused
                | ErrorKind::BrokenPipe
                | ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
}

/// For a service whose 429 responses are always temporary.
pub(crate) fn never_exhausted(_body: &Value) -> bool {
    false
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::VecDeque, io::Read};

    use adamantium::Actor;
    use serde_json::json;

    use super::*;

    const RETRY_BUDGET: Duration = Duration::from_secs(180);

    pub(crate) type StubResponse = (u16, Option<&'static str>, &'static str);

    /// An agent that answers each request with the next canned response and
    /// counts the requests.
    pub(crate) fn stub_agent(responses: Vec<StubResponse>) -> (ureq::Agent, Actor<usize>) {
        let responses = Actor::new(VecDeque::from(responses));
        let calls = Actor::new(0);
        let captured_calls = calls.clone();
        let config = ureq::Agent::config_builder()
            .middleware(
                move |_request: ureq::http::Request<ureq::SendBody>,
                      _next: ureq::middleware::MiddlewareNext| {
                    captured_calls.with(|calls| *calls += 1);
                    let (status, retry_after, body) = responses.with(VecDeque::pop_front).unwrap();
                    let mut response = ureq::http::Response::builder().status(status);
                    if let Some(retry_after) = retry_after {
                        response = response.header("Retry-After", retry_after);
                    }
                    Ok(response
                        .body(
                            ureq::Body::builder()
                                .mime_type("application/json")
                                .data(body),
                        )
                        .unwrap())
                },
            )
            .build();
        (config.into(), calls)
    }

    fn service(agent: ureq::Agent) -> JsonService {
        JsonService::with_agent(
            agent,
            "Test",
            "https://api.example.test/v1/answer",
            vec![("x-api-key", "local-test-key".to_owned())],
            Duration::from_secs(60),
            RETRY_BUDGET,
            |body| body["error"]["type"] == "insufficient_quota",
        )
    }

    fn stub_service(responses: Vec<StubResponse>) -> (JsonService, Actor<usize>) {
        let (agent, calls) = stub_agent(responses);
        (service(agent), calls)
    }

    #[test]
    fn retries_gateway_timeouts_with_exponential_backoff_and_jitter() {
        let (client, calls) = stub_service(vec![
            (504, None, ""),
            (504, None, ""),
            (200, None, r#"{"answers":[]}"#),
        ]);
        let mut delays = Vec::new();
        let response = client
            .post_with(
                &json!({}),
                RETRY_BUDGET,
                |delay| delays.push(delay),
                || Duration::from_millis(125),
            )
            .unwrap();
        assert_eq!(response["answers"], json!([]));
        assert_eq!(calls.with(|calls| *calls), 3);
        assert_eq!(
            delays,
            [Duration::from_millis(1125), Duration::from_millis(2125)]
        );
    }

    #[test]
    fn honors_retry_after_and_defers_when_it_exceeds_the_limit() {
        let (client, calls) = stub_service(vec![
            (429, Some("3"), r#"{"error":{"type":"rate_limit_error"}}"#),
            (200, None, r#"{"answers":[]}"#),
        ]);
        let mut delays = Vec::new();
        client
            .post_with(
                &json!({}),
                RETRY_BUDGET,
                |delay| delays.push(delay),
                || Duration::from_millis(100),
            )
            .unwrap();
        assert_eq!(delays, [Duration::from_millis(3100)]);
        assert_eq!(calls.with(|calls| *calls), 2);

        let (client, calls) = stub_service(vec![(503, Some("31"), "")]);
        let error = client
            .post_with(
                &json!({}),
                RETRY_BUDGET,
                |_| panic!("should defer"),
                Duration::default,
            )
            .unwrap_err();
        assert!(error.to_string().contains("retry deferred"));
        assert!(error.to_string().contains("Test API returned HTTP 503"));
        assert_eq!(calls.with(|calls| *calls), 1);

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let date = httpdate::fmt_http_date(now + Duration::from_secs(7));
        assert_eq!(parse_retry_after(&date, now), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after("999999999999999999999", now),
            Some(Duration::from_secs(u64::MAX))
        );
        assert_eq!(parse_retry_after("invalid", now), None);
    }

    #[test]
    fn retries_are_bounded_and_permanent_failures_are_not_retried() {
        let (client, calls) = stub_service(vec![(504, None, ""); MAX_ATTEMPTS]);
        let mut delays = Vec::new();
        let error = client
            .post_with(
                &json!({}),
                RETRY_BUDGET,
                |delay| delays.push(delay),
                Duration::default,
            )
            .unwrap_err();
        assert!(error.to_string().contains("HTTP 504"));
        assert_eq!(calls.with(|calls| *calls), MAX_ATTEMPTS);
        assert_eq!(
            delays,
            [
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4)
            ]
        );

        for response in [
            (401, None, ""),
            (429, None, r#"{"error":{"type":"insufficient_quota"}}"#),
            (200, None, "not JSON"),
        ] {
            let (client, calls) = stub_service(vec![response]);
            assert!(
                client
                    .post_with(
                        &json!({}),
                        RETRY_BUDGET,
                        |_| panic!("should not retry"),
                        Duration::default
                    )
                    .is_err()
            );
            assert_eq!(calls.with(|calls| *calls), 1);
        }
    }

    #[test]
    fn errors_carry_the_services_explanation() {
        let (client, _) = stub_service(vec![(
            404,
            None,
            r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-nope"}}"#,
        )]);
        let error = client
            .post_with(
                &json!({}),
                RETRY_BUDGET,
                |_| panic!("should not retry"),
                Duration::default,
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Test API returned HTTP 404 Not Found: model: claude-nope (after 1 attempt(s))"
        );
    }

    #[test]
    fn stops_if_the_next_backoff_would_exceed_the_time_budget() {
        let (client, calls) = stub_service(vec![(504, None, "")]);
        let error = client
            .post_with(
                &json!({}),
                Duration::from_millis(500),
                |_| panic!("should not sleep beyond the retry budget"),
                Duration::default,
            )
            .unwrap_err();
        assert!(error.to_string().contains("retry budget exhausted"));
        assert_eq!(calls.with(|calls| *calls), 1);
    }

    #[test]
    fn only_retries_transient_transport_errors() {
        for kind in [
            ErrorKind::TimedOut,
            ErrorKind::ConnectionReset,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(transient_transport_error(&ureq::Error::Io(kind.into())));
        }
        assert!(!transient_transport_error(&ureq::Error::Io(
            ErrorKind::InvalidInput.into()
        )));
        assert!(!transient_transport_error(&ureq::Error::Tls(
            "invalid certificate"
        )));
    }

    #[test]
    fn retries_connection_failures_while_sending_and_reading() {
        struct BrokenReader;
        impl Read for BrokenReader {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(ErrorKind::ConnectionReset.into())
            }
        }
        let calls = Actor::new(0);
        let captured_calls = calls.clone();
        let config = ureq::Agent::config_builder()
            .middleware(
                move |_request: ureq::http::Request<ureq::SendBody>,
                      _next: ureq::middleware::MiddlewareNext| {
                    let call = captured_calls.with(|calls| {
                        *calls += 1;
                        *calls
                    });
                    match call {
                        1 => Err(ureq::Error::ConnectionFailed),
                        2 => Ok(ureq::http::Response::builder()
                            .status(200)
                            .body(ureq::Body::builder().reader(BrokenReader))
                            .unwrap()),
                        _ => Ok(ureq::http::Response::builder()
                            .status(200)
                            .body(ureq::Body::builder().data(r#"{"answers":[]}"#))
                            .unwrap()),
                    }
                },
            )
            .build();
        let client = service(config.into());
        let mut delays = Vec::new();
        client
            .post_with(
                &json!({}),
                RETRY_BUDGET,
                |delay| delays.push(delay),
                Duration::default,
            )
            .unwrap();
        assert_eq!(calls.with(|calls| *calls), 3);
        assert_eq!(delays, [Duration::from_secs(1), Duration::from_secs(2)]);
    }

    #[test]
    fn sends_the_configured_headers_and_keeps_them_out_of_the_body() {
        let captured = Actor::new(None);
        let destination = captured.clone();
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .middleware(
                move |request: ureq::http::Request<ureq::SendBody>,
                      _next: ureq::middleware::MiddlewareNext| {
                    let (parts, body) = request.into_parts();
                    let mut data = String::new();
                    body.into_reader().read_to_string(&mut data).unwrap();
                    destination.with(|captured| *captured = Some((parts, data)));
                    Ok(ureq::http::Response::builder()
                        .status(200)
                        .body(ureq::Body::builder().data(r#"{"ok":true}"#))
                        .unwrap())
                },
            )
            .build();
        let response = service(config.into())
            .post(&json!({ "model": "test-model" }))
            .unwrap();
        assert_eq!(response["ok"], true);
        let (parts, body) = captured.with(Option::take).unwrap();
        assert_eq!(parts.method, "POST");
        assert_eq!(parts.uri.to_string(), "https://api.example.test/v1/answer");
        assert_eq!(parts.headers["x-api-key"], "local-test-key");
        assert_eq!(parts.headers["content-type"], "application/json");
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({ "model": "test-model" })
        );
    }
}
