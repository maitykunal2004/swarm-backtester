use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

/// Minimal HTTP response abstraction so the fetcher can be tested without
/// network access.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub headers: BTreeMap<String, String>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Blocking HTTP transport. Implementations must not retry on their own: the
/// fetcher owns rate-limit, backoff and retry policy.
pub trait HttpTransport: Send + Sync {
    #[allow(clippy::ptr_arg)]
    fn get(
        &self,
        url: &str,
        query: &[(&str, String)],
        headers: &[(&str, String)],
    ) -> Result<HttpResponse, String>;
}

pub struct UreqTransport {
    agent: ureq::Agent,
    max_body_bytes: usize,
}

impl UreqTransport {
    pub fn new(timeout: Duration, max_body_bytes: usize) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout)
            .user_agent("swarm-backtester/0.1 (research; keyless)")
            .build();
        Self {
            agent,
            max_body_bytes,
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new(Duration::from_secs(20), 8 * 1024 * 1024)
    }
}

impl HttpTransport for UreqTransport {
    fn get(
        &self,
        url: &str,
        query: &[(&str, String)],
        headers: &[(&str, String)],
    ) -> Result<HttpResponse, String> {
        let mut request = self.agent.get(url);
        for (key, value) in query {
            request = request.query(key, value);
        }
        for (key, value) in headers {
            request = request.set(key, value);
        }
        match request.call() {
            Ok(response) => read_response(response, self.max_body_bytes),
            Err(ureq::Error::Status(_, response)) => read_response(response, self.max_body_bytes),
            Err(ureq::Error::Transport(err)) => Err(err.to_string()),
        }
    }
}

fn read_response(response: ureq::Response, max_body_bytes: usize) -> Result<HttpResponse, String> {
    let status = response.status();
    let mut headers = BTreeMap::new();
    for name in [
        "retry-after",
        "x-mbx-used-weight-1m",
        "content-type",
        "content-length",
    ] {
        if let Some(value) = response.header(name) {
            headers.insert(name.to_string(), value.to_string());
        }
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take(max_body_bytes as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|err| format!("failed to read response body: {err}"))?;
    if body.len() > max_body_bytes {
        return Err(format!(
            "response body exceeds configured limit of {max_body_bytes} bytes"
        ));
    }
    Ok(HttpResponse {
        status,
        body,
        headers,
    })
}
