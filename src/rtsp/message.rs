//! RTSP requests and the answers to them, which are HTTP's shape with a
//! sequence number in place of a connection.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    /// Header names are not case sensitive, and cameras disagree about them.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn ok(&self) -> bool {
        self.status == 200
    }

    /// The session id, without the timeout that may be written beside it.
    pub fn session(&self) -> Option<&str> {
        let session = self.header("Session")?;

        Some(match session.split_once(';') {
            Some((id, _)) => id.trim(),
            None => session.trim(),
        })
    }

    /// How long the camera will hold the session with nothing being asked.
    pub fn session_timeout(&self) -> Option<u64> {
        let session = self.header("Session")?;

        session.split(';').find_map(|part| {
            let (name, value) = part.split_once('=')?;

            name.trim()
                .eq_ignore_ascii_case("timeout")
                .then(|| value.trim().parse().ok())?
        })
    }

    /// How much body follows the headers, zero when nothing says.
    pub fn content_length(&self) -> usize {
        self.header("Content-Length")
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    }
}

/// Reads the head of a response: the status line and the headers.
///
/// None if `head` is not an RTSP response at all, which is how a camera
/// speaking something else is found out.
pub fn parse(head: &str) -> Option<Response> {
    let mut lines = head.lines();
    let status_line = lines.next()?;

    let mut fields = status_line.splitn(3, ' ');
    let version = fields.next()?;

    if !version.starts_with("RTSP/") {
        return None;
    }

    let status = fields.next()?.parse().ok()?;
    let reason = fields.next().unwrap_or("").trim().to_string();

    let mut headers = Vec::new();

    for line in lines {
        let line = line.trim_end();

        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    Some(Response {
        status,
        reason,
        headers,
        body: String::new(),
    })
}

/// One request, as it goes out on the wire.
#[derive(Clone)]
pub struct Request {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn new(method: &str, uri: &str) -> Self {
        Self {
            method: method.to_string(),
            uri: uri.to_string(),
            headers: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn to_wire(&self, sequence: u32) -> String {
        let mut text = format!(
            "{} {} RTSP/1.0\r\nCSeq: {sequence}\r\n",
            self.method, self.uri
        );

        for (name, value) in &self.headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }

        text.push_str("\r\n");
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETUP: &str = "RTSP/1.0 200 OK\r\n\
         CSeq: 3\r\n\
         Session: 12345678;timeout=60\r\n\
         Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n";

    #[test]
    fn an_answer_is_read_down_to_its_headers() {
        let response = parse(SETUP).expect("an answer");

        assert!(response.ok());
        assert_eq!(response.reason, "OK");
        assert_eq!(response.session(), Some("12345678"));
        assert_eq!(response.session_timeout(), Some(60));
        assert_eq!(
            response.header("transport"),
            Some("RTP/AVP/TCP;unicast;interleaved=0-1")
        );
    }

    #[test]
    fn a_session_without_a_timeout_is_still_a_session() {
        let response = parse("RTSP/1.0 200 OK\r\nSession: ABCD\r\n\r\n").expect("an answer");

        assert_eq!(response.session(), Some("ABCD"));
        assert_eq!(response.session_timeout(), None);
    }

    #[test]
    fn what_was_refused_says_why() {
        let response = parse(
            "RTSP/1.0 401 Unauthorized\r\n\
             WWW-Authenticate: Digest realm=\"cam\", nonce=\"xyz\"\r\n\r\n",
        )
        .expect("an answer");

        assert_eq!(response.status, 401);
        assert!(!response.ok());
        assert!(response.header("WWW-Authenticate").is_some());
    }

    #[test]
    fn a_body_is_measured_before_it_is_read() {
        let response = parse("RTSP/1.0 200 OK\r\nContent-Length: 42\r\n\r\n").expect("an answer");

        assert_eq!(response.content_length(), 42);
        assert_eq!(
            parse("RTSP/1.0 200 OK\r\n\r\n")
                .expect("an answer")
                .content_length(),
            0,
            "nothing said means nothing follows"
        );
    }

    #[test]
    fn what_is_not_an_answer_is_refused() {
        assert!(parse("").is_none());
        assert!(parse("HTTP/1.1 200 OK\r\n\r\n").is_none(), "not rtsp");
        assert!(parse("RTSP/1.0 nonsense\r\n\r\n").is_none());
    }

    #[test]
    fn a_request_goes_out_with_its_sequence_number() {
        let request = Request::new("DESCRIBE", "rtsp://cam:554/s")
            .header("Accept", "application/sdp")
            .to_wire(2);

        assert_eq!(
            request,
            "DESCRIBE rtsp://cam:554/s RTSP/1.0\r\n\
             CSeq: 2\r\n\
             Accept: application/sdp\r\n\r\n"
        );
    }
}
