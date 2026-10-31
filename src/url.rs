//! The little of a URL an RTSP client needs.
//!
//! Credentials may be written into the address, as every camera's own page
//! does, and they are kept out of the request line and the logs.

use std::format;

/// The default port for RTSP, as every camera uses unless told otherwise.
const PORT: u16 = 554;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,

    /// Path and query, as written, leading slash and all.
    pub path: String,

    pub user: Option<String>,
    pub password: Option<String>,
}

impl Url {
    /// `rtsp://host:port/path`, which is what goes on a request line: the
    /// credentials belong in a header, never here.
    pub fn request_uri(&self) -> String {
        format!("rtsp://{}:{}{}", self.authority(), self.port, self.path)
    }

    /// The host as an address is written: in brackets when it is IPv6, or the
    /// colons of the address and of the port could not be told apart.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            return format!("[{}]", self.host);
        }

        self.host.clone()
    }

    /// Resolves what SDP gave as a track's control address.
    pub fn control_uri(&self, control: &str) -> String {
        if control.is_empty() || control == "*" {
            return self.request_uri();
        }

        // An absolute address is the camera's own word on where the track is.
        if control.contains("://") {
            return control.to_string();
        }

        let base = self.request_uri();

        match (base.ends_with('/'), control.starts_with('/')) {
            (true, true) => format!("{}{}", base.trim_end_matches('/'), control),
            (false, false) => format!("{base}/{control}"),
            _ => format!("{base}{control}"),
        }
    }
}

/// None if this is not an address an RTSP client can dial.
pub fn parse(text: &str) -> Option<Url> {
    let text = text.trim();
    let rest = strip_scheme(text)?;

    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };

    let (credentials, host_port) = match authority.rfind('@') {
        Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
        None => (None, authority),
    };

    let (user, password) = match credentials {
        Some(text) => match text.split_once(':') {
            Some((user, password)) => (some(user), some(password)),
            None => (some(text), None),
        },
        None => (None, None),
    };

    let (host, port) = split_host(host_port)?;

    Some(Url {
        host,
        port,
        path: path.to_string(),
        user,
        password,
    })
}

fn strip_scheme(text: &str) -> Option<&str> {
    let at = text.find("://")?;
    let scheme = &text[..at];

    // rtsps is RTSP over TLS, which this plugin does not speak.
    scheme
        .eq_ignore_ascii_case("rtsp")
        .then_some(&text[at + 3..])
}

/// Splits `host:port`, taking an address in brackets as one piece.
fn split_host(text: &str) -> Option<(String, u16)> {
    if text.is_empty() {
        return None;
    }

    if let Some(end) = text.strip_prefix('[').and_then(|rest| rest.find(']')) {
        let host = text[1..end + 1].to_string();
        let rest = &text[end + 2..];

        let port = match rest.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None => PORT,
        };

        return Some((host, port));
    }

    match text.split_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None => Some((text.to_string(), PORT)),
    }
}

fn some(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_read_down_to_its_parts() {
        let url = parse("rtsp://camera.local:8554/stream1?channel=2").expect("an address");

        assert_eq!(url.host, "camera.local");
        assert_eq!(url.port, 8554);
        assert_eq!(url.path, "/stream1?channel=2");
        assert_eq!(url.user, None);
    }

    #[test]
    fn the_port_and_path_have_defaults() {
        let url = parse("rtsp://10.0.0.7").expect("an address");

        assert_eq!(url.port, 554);
        assert_eq!(url.path, "/");
        assert_eq!(url.request_uri(), "rtsp://10.0.0.7:554/");
    }

    /// Cameras hand out addresses with the login in them, and the login must
    /// not come back out on the request line.
    #[test]
    fn credentials_in_the_address_are_taken_out_of_it() {
        let url = parse("rtsp://admin:pa%40ss@10.0.0.7/h264").expect("an address");

        assert_eq!(url.user.as_deref(), Some("admin"));
        assert_eq!(url.password.as_deref(), Some("pa%40ss"));
        assert_eq!(url.request_uri(), "rtsp://10.0.0.7:554/h264");
    }

    #[test]
    fn a_user_without_a_password_is_still_a_user() {
        let url = parse("rtsp://admin@10.0.0.7/").expect("an address");

        assert_eq!(url.user.as_deref(), Some("admin"));
        assert_eq!(url.password, None);
    }

    #[test]
    fn an_address_in_brackets_keeps_its_colons() {
        let url = parse("rtsp://[fe80::1]:8554/live").expect("an address");

        assert_eq!(url.host, "fe80::1");
        assert_eq!(url.port, 8554);
        assert_eq!(url.request_uri(), "rtsp://[fe80::1]:8554/live");
    }

    #[test]
    fn what_is_not_an_rtsp_address_is_refused() {
        assert!(parse("http://camera/stream").is_none());
        assert!(parse("rtsps://camera/stream").is_none(), "no tls here");
        assert!(parse("camera/stream").is_none());
        assert!(parse("rtsp://").is_none());
    }

    #[test]
    fn a_track_is_found_beside_the_stream_or_wherever_it_says() {
        let url = parse("rtsp://cam/h264").expect("an address");

        assert_eq!(
            url.control_uri("trackID=1"),
            "rtsp://cam:554/h264/trackID=1"
        );
        assert_eq!(url.control_uri("/track1"), "rtsp://cam:554/h264/track1");
        assert_eq!(url.control_uri("*"), "rtsp://cam:554/h264");
        assert_eq!(
            url.control_uri("rtsp://other:1/t"),
            "rtsp://other:1/t",
            "an absolute control address is the camera's word"
        );
    }

    /// A stream whose path already ends in a slash must not gain a second.
    #[test]
    fn a_track_under_a_trailing_slash_joins_cleanly() {
        let url = parse("rtsp://cam/h264/").expect("an address");

        assert_eq!(
            url.control_uri("trackID=1"),
            "rtsp://cam:554/h264/trackID=1"
        );
        assert_eq!(
            url.control_uri("/trackID=1"),
            "rtsp://cam:554/h264/trackID=1"
        );
    }
}
