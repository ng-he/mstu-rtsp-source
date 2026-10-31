//! An RTSP client, speaking only what a camera needs to hear.
//!
//! RTP comes back inside the same connection, interleaved with the answers,
//! so there is one socket to manage and nothing for a firewall to drop.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;
use std::{matches, vec};

use mstu_media::sdp;

use crate::url::Url;

pub mod auth;
pub mod message;

pub use auth::{Challenge, Credentials};
pub use message::{Request, Response};

/// Which interleaved channels the track's data and its reports arrive on.
pub const DATA_CHANNEL: u8 = 0;
pub const REPORT_CHANNEL: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// Nothing arrived before the read timeout, which is not yet a failure:
    /// the caller decides whether to wait longer or give up.
    #[error("nothing arrived")]
    Idle,

    #[error("the camera answered {0} {1}")]
    Status(u16, String),

    #[error("the login was refused")]
    Unauthorized,

    #[error("the camera needs a login")]
    NeedsLogin,

    #[error("{0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// What came in on the connection.
#[derive(Debug)]
pub enum Received {
    Data { channel: u8, bytes: Vec<u8> },
    Answer(Response),
}

pub struct Session {
    reader: BufReader<TcpStream>,

    /// The stream's own address, which every request is about.
    uri: String,

    credentials: Option<Credentials>,
    challenge: Option<Challenge>,

    /// How many answers this challenge has had, which digest counts.
    answers: u32,

    sequence: u32,
    session: Option<String>,

    /// How long the camera holds the session while nothing is asked of it.
    pub timeout: Duration,
}

impl Session {
    pub fn connect(
        url: &Url,
        credentials: Option<Credentials>,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self> {
        let address = format!("{}:{}", url.authority(), url.port);

        // Resolving and connecting are separate so a name that does not
        // resolve fails here rather than hanging on the socket.
        let mut addresses = address
            .to_socket_addrs()
            .map_err(|error| Error::Protocol(format!("'{address}' is nowhere: {error}")))?;

        let first = addresses
            .next()
            .ok_or_else(|| Error::Protocol(format!("'{address}' is nowhere")))?;

        let stream = TcpStream::connect_timeout(&first, connect_timeout)?;

        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_nodelay(true)?;

        Ok(Self {
            reader: BufReader::new(stream),
            uri: url.request_uri(),
            credentials,
            challenge: None,
            answers: 0,
            sequence: 0,
            session: None,
            timeout: Duration::from_secs(60),
        })
    }

    /// What the camera offers, as SDP describes it.
    pub fn describe(&mut self) -> Result<sdp::Description> {
        let uri = self.uri.clone();
        let answer =
            self.ask(Request::new("DESCRIBE", &uri).header("Accept", "application/sdp"))?;

        Ok(sdp::parse(&answer.body))
    }

    /// Asks for one track to arrive inside this connection.
    pub fn setup(&mut self, control: &str) -> Result<()> {
        let transport = format!("RTP/AVP/TCP;unicast;interleaved={DATA_CHANNEL}-{REPORT_CHANNEL}");
        let answer = self.ask(Request::new("SETUP", control).header("Transport", &transport))?;

        // Without a session id nothing afterwards can be addressed.
        let Some(session) = answer.session() else {
            return Err(Error::Protocol("the camera gave no session".to_string()));
        };

        self.session = Some(session.to_string());

        if let Some(seconds) = answer.session_timeout() {
            self.timeout = Duration::from_secs(seconds.max(1));
        }

        Ok(())
    }

    pub fn play(&mut self) -> Result<()> {
        let uri = self.uri.clone();

        self.ask(Request::new("PLAY", &uri).header("Range", "npt=0.000-"))?;

        Ok(())
    }

    /// Says the client is still here. The answer arrives among the data and
    /// is of no interest, so this does not wait for it.
    pub fn keepalive(&mut self) -> Result<()> {
        let uri = self.uri.clone();
        let request = self.prepare(Request::new("OPTIONS", &uri));

        self.send(&request)
    }

    /// Lets the camera go, if it is listening.
    pub fn teardown(&mut self) {
        let uri = self.uri.clone();
        let request = self.prepare(Request::new("TEARDOWN", &uri));

        let _ = self.send(&request);
    }

    /// The next thing on the connection, data or an answer.
    pub fn receive(&mut self) -> Result<Received> {
        let first = self.byte()?;

        if first != b'$' {
            return Ok(Received::Answer(self.answer(first)?));
        }

        let mut head = [0u8; 3];

        self.reader.read_exact(&mut head)?;

        let channel = head[0];
        let length = u16::from_be_bytes([head[1], head[2]]) as usize;

        let mut bytes = vec![0u8; length];

        self.reader.read_exact(&mut bytes)?;

        Ok(Received::Data { channel, bytes })
    }

    /// Sends a request and waits for its answer, logging in if the camera
    /// asks and there is a login to give.
    fn ask(&mut self, request: Request) -> Result<Response> {
        let prepared = self.prepare(request.clone());

        self.send(&prepared)?;

        let answer = self.wait()?;

        if answer.status != 401 {
            return self.checked(answer);
        }

        let Some(header) = answer.header("WWW-Authenticate") else {
            return Err(Error::Unauthorized);
        };

        let Some(challenge) = Challenge::parse(header) else {
            return Err(Error::Protocol(format!(
                "a login of a kind we cannot give: {header}"
            )));
        };

        if self.credentials.is_none() {
            return Err(Error::NeedsLogin);
        }

        // A second refusal is a wrong password, not a challenge to answer.
        let again = self.challenge.as_ref() == Some(&challenge);

        self.challenge = Some(challenge);
        self.answers = 0;

        if again {
            return Err(Error::Unauthorized);
        }

        let prepared = self.prepare(request);

        self.send(&prepared)?;

        let answer = self.wait()?;

        if answer.status == 401 {
            return Err(Error::Unauthorized);
        }

        self.checked(answer)
    }

    fn checked(&self, answer: Response) -> Result<Response> {
        if answer.ok() {
            return Ok(answer);
        }

        Err(Error::Status(answer.status, answer.reason.clone()))
    }

    /// Adds what every request carries, and the login when one is in hand.
    fn prepare(&mut self, mut request: Request) -> String {
        self.sequence += 1;

        request = request.header(
            "User-Agent",
            concat!("mstu-rtsp-source/", env!("CARGO_PKG_VERSION")),
        );

        if let Some(session) = &self.session {
            request = request.header("Session", session);
        }

        if let (Some(challenge), Some(credentials)) = (&self.challenge, &self.credentials) {
            self.answers += 1;

            let header = challenge.answer(credentials, &request.method, &request.uri, self.answers);

            request = request.header("Authorization", &header);
        }

        request.to_wire(self.sequence)
    }

    fn send(&mut self, request: &str) -> Result<()> {
        let mut socket = self.reader.get_ref();

        socket.write_all(request.as_bytes())?;
        socket.flush()?;

        Ok(())
    }

    /// The next answer, with any data that arrives before it dropped: during
    /// the handshake there is none, and afterwards it is a keepalive's.
    fn wait(&mut self) -> Result<Response> {
        loop {
            match self.receive()? {
                Received::Answer(answer) => return Ok(answer),
                Received::Data { .. } => continue,
            }
        }
    }

    /// Reads one answer, given the first byte of it.
    fn answer(&mut self, first: u8) -> Result<Response> {
        let mut head = String::new();

        head.push(first as char);
        self.reader.read_line(&mut head)?;

        loop {
            let mut line = String::new();

            if self.reader.read_line(&mut line)? == 0 {
                break;
            }

            let empty = line.trim().is_empty();

            head.push_str(&line);

            if empty {
                break;
            }
        }

        let Some(mut answer) = message::parse(&head) else {
            return Err(Error::Protocol(format!("not an answer: {:?}", head.trim())));
        };

        let length = answer.content_length();

        if length > 0 {
            let mut body = vec![0u8; length];

            self.reader.read_exact(&mut body)?;
            answer.body = String::from_utf8_lossy(&body).to_string();
        }

        Ok(answer)
    }

    /// One byte, with a read timeout taken as quiet rather than as failure.
    ///
    /// Only ever called where nothing is half read, so a timeout here loses
    /// no place in the stream.
    fn byte(&mut self) -> Result<u8> {
        let mut one = [0u8; 1];

        match self.reader.read_exact(&mut one) {
            Ok(()) => Ok(one[0]),
            Err(error) if quiet(&error) => Err(Error::Idle),
            Err(error) => Err(error.into()),
        }
    }
}

/// Whether an error is the read timeout coming round rather than a failure.
fn quiet(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{Camera, Demand, idr, interleaved, rtp};

    fn dial(camera: &Camera, user: Option<&str>) -> Session {
        let url = crate::url::parse(&camera.url()).expect("the camera's address");

        let credentials = user.map(|user| Credentials {
            user: user.to_string(),
            password: "1234".to_string(),
        });

        Session::connect(
            &url,
            credentials,
            Duration::from_secs(2),
            Duration::from_millis(300),
        )
        .expect("a connection to the camera")
    }

    #[test]
    fn a_stream_is_described_set_up_and_played() {
        let camera = Camera::start(Demand::Nothing, vec![]);
        let mut session = dial(&camera, None);

        let description = session.describe().expect("a description");
        let video = description.video().expect("a video track").clone();

        assert_eq!(video.encoding, "H264");

        let track = crate::url::parse(&camera.url())
            .expect("the camera's address")
            .control_uri(&video.control);

        session.setup(&track).expect("the track set up");

        session.play().expect("the stream playing");

        let asked = camera.asked();

        assert!(asked[0].starts_with("DESCRIBE"), "{asked:?}");
        assert!(asked[1].starts_with("SETUP rtsp://"), "{asked:?}");
        assert!(asked[2].starts_with("PLAY"), "{asked:?}");
    }

    #[test]
    fn the_packets_come_back_inside_the_connection() {
        let frames = vec![interleaved(
            DATA_CHANNEL,
            &rtp(96, 1, 9000, true, &idr(&[1, 2, 3])),
        )];

        let camera = Camera::start(Demand::Nothing, frames);
        let mut session = dial(&camera, None);

        session.describe().expect("a description");

        let uri = session.uri.clone();

        session.setup(&uri).expect("the track set up");
        session.play().expect("the stream playing");

        match session.receive().expect("a frame") {
            Received::Data { channel, bytes } => {
                assert_eq!(channel, DATA_CHANNEL);
                let packet = mstu_media::rtp::parse(&bytes).expect("a packet");

                assert_eq!(packet.timestamp, 9000);
            }
            other => panic!("expected data, got {other:?}"),
        }
    }

    /// A camera with nothing to say is quiet, which is not a failure.
    #[test]
    fn a_quiet_connection_says_so_rather_than_failing() {
        let camera = Camera::start(Demand::Nothing, vec![]);
        let mut session = dial(&camera, None);

        session.describe().expect("a description");

        assert!(matches!(session.receive(), Err(Error::Idle)));
    }

    #[test]
    fn a_camera_that_wants_a_basic_login_gets_one() {
        let camera = Camera::start(Demand::Basic, vec![]);
        let mut session = dial(&camera, Some("admin"));

        session.describe().expect("a description");

        assert_eq!(camera.asked().len(), 2, "refused once, then answered");
    }

    #[test]
    fn a_camera_that_wants_a_digest_login_gets_one() {
        let camera = Camera::start(Demand::Digest, vec![]);
        let mut session = dial(&camera, Some("admin"));

        session.describe().expect("a description");

        assert_eq!(camera.asked().len(), 2);
    }

    /// Without a login to give there is nothing to retry, and the plugin has
    /// to say which of the two it is.
    #[test]
    fn a_camera_that_wants_a_login_and_is_given_none_says_so() {
        let camera = Camera::start(Demand::Digest, vec![]);
        let mut session = dial(&camera, None);

        assert!(matches!(session.describe(), Err(Error::NeedsLogin)));
    }
}
