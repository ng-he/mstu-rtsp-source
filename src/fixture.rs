//! Packets, descriptions and a camera to test against.
//!
//! Everything here is built byte by byte, so a test says what it means and no
//! network is needed to run one.

use std::format;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The units, packets and descriptions a camera sends are built by the media
/// crate, which is where every plugin's tests get them.
pub use mstu_media::fixture::{PPS, SPS, description, h265, idr, rtp, slice};

/// An RTP packet in its interleaved frame, as it travels over the connection.
pub fn interleaved(channel: u8, packet: &[u8]) -> Vec<u8> {
    let mut frame = vec![b'$', channel];

    frame.extend_from_slice(&(packet.len() as u16).to_be_bytes());
    frame.extend_from_slice(packet);

    frame
}

/// What a camera should ask of a client before it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demand {
    Nothing,
    Basic,
    Digest,
}

/// A camera: it answers the handshake, sends what it was given, and
/// remembers what was asked of it.
///
/// It takes one connection at a time and as many in turn as it is given, so a
/// client that drops and dials again is answered the same way.
pub struct Camera {
    pub address: SocketAddr,
    asked: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    serving: Option<JoinHandle<()>>,
}

impl Camera {
    /// `frames` go out once PLAY has been answered, in order.
    pub fn start(demand: Demand, frames: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port of our own");
        let address = listener.local_addr().expect("the port it took");

        let asked = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let serving = {
            let asked = Arc::clone(&asked);
            let stop = Arc::clone(&stop);

            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((stream, _)) = listener.accept() else {
                        return;
                    };

                    // One client at a time, each answered to the end.
                    serve(stream, demand, frames.clone(), &asked, &stop);
                }
            })
        };

        Self {
            address,
            asked,
            stop,
            serving: Some(serving),
        }
    }

    pub fn url(&self) -> String {
        format!("rtsp://{}/stream", self.address)
    }

    /// How many times a method has been asked for, over all connections.
    pub fn asked_for(&self, method: &str) -> usize {
        self.asked()
            .iter()
            .filter(|line| line.starts_with(method))
            .count()
    }

    /// Every request line the camera was sent, in order.
    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("what was asked").clone()
    }

    /// Waits until `method` has been asked for `times` times, so a test need
    /// not sleep for as long as the worst case.
    pub fn waited_for(&self, method: &str, times: usize, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;

        while std::time::Instant::now() < deadline {
            if self.asked_for(method) >= times {
                return true;
            }

            thread::sleep(Duration::from_millis(10));
        }

        false
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        // The connection going is what ends the thread, whatever it waits on.
        if let Some(serving) = self.serving.take() {
            let _ = TcpStream::connect(self.address);
            let _ = serving.join();
        }
    }
}

fn serve(
    stream: TcpStream,
    demand: Demand,
    frames: Vec<Vec<u8>>,
    asked: &Arc<Mutex<Vec<String>>>,
    stop: &Arc<AtomicBool>,
) {
    let mut socket = stream.try_clone().expect("a handle to answer on");
    let mut lines = BufReader::new(stream);

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }

        let Some(request) = read_request(&mut lines) else {
            return;
        };

        let line = request.first().cloned().unwrap_or_default();
        let method = line.split(' ').next().unwrap_or("").to_string();

        asked.lock().expect("what was asked").push(line);

        let sequence = header(&request, "CSeq").unwrap_or_else(|| "0".to_string());
        let authorized = request
            .iter()
            .any(|line| line.starts_with("Authorization:"));

        // Everything but OPTIONS needs the login, when the camera wants one.
        if demand != Demand::Nothing && !authorized && method != "OPTIONS" {
            let challenge = match demand {
                Demand::Basic => "Basic realm=\"fixture\"".to_string(),
                _ => "Digest realm=\"fixture\", nonce=\"abc123\"".to_string(),
            };

            let _ = write!(
                socket,
                "RTSP/1.0 401 Unauthorized\r\n\
                 CSeq: {sequence}\r\n\
                 WWW-Authenticate: {challenge}\r\n\r\n"
            );

            continue;
        }

        match method.as_str() {
            "DESCRIBE" => {
                let body = description();

                let _ = write!(
                    socket,
                    "RTSP/1.0 200 OK\r\n\
                     CSeq: {sequence}\r\n\
                     Content-Type: application/sdp\r\n\
                     Content-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }

            "SETUP" => {
                let _ = write!(
                    socket,
                    "RTSP/1.0 200 OK\r\n\
                     CSeq: {sequence}\r\n\
                     Session: 12345678;timeout=60\r\n\
                     Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
                );
            }

            "PLAY" => {
                let _ = write!(
                    socket,
                    "RTSP/1.0 200 OK\r\n\
                     CSeq: {sequence}\r\n\
                     Session: 12345678\r\n\r\n"
                );

                for frame in &frames {
                    if stop.load(Ordering::Relaxed) || socket.write_all(frame).is_err() {
                        return;
                    }
                }
            }

            "TEARDOWN" => {
                let _ = write!(socket, "RTSP/1.0 200 OK\r\nCSeq: {sequence}\r\n\r\n");
                return;
            }

            _ => {
                let _ = write!(
                    socket,
                    "RTSP/1.0 200 OK\r\n\
                     CSeq: {sequence}\r\n\
                     Public: DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n"
                );
            }
        }
    }
}

/// The lines of one request, and the body read off if it has one.
fn read_request(lines: &mut BufReader<TcpStream>) -> Option<Vec<String>> {
    let mut request = Vec::new();

    loop {
        let mut line = String::new();

        if lines.read_line(&mut line).ok()? == 0 {
            return None;
        }

        let line = line.trim_end().to_string();

        if line.is_empty() {
            break;
        }

        request.push(line);
    }

    if let Some(length) = header(&request, "Content-Length").and_then(|v| v.parse().ok()) {
        let mut body = vec![0u8; length];
        lines.read_exact(&mut body).ok()?;
    }

    (!request.is_empty()).then_some(request)
}

fn header(request: &[String], name: &str) -> Option<String> {
    request.iter().find_map(|line| {
        let (key, value) = line.split_once(':')?;

        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}
