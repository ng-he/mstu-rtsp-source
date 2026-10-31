//! The thread that keeps a camera's stream flowing.
//!
//! Nothing here touches the host: the engine's own thread does that when it
//! comes to collect, in `process`. This side only reads the network, puts
//! frames in a queue and says what it is doing.

use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mstu_media::rtp::{self, Clock, Depacketizer};
use mstu_sdk::{Logger, log_debug, log_info, log_warn};

use crate::rtsp::{self, Credentials, DATA_CHANNEL, Received, Session};
use crate::url;

/// How long a read waits before the loop looks around: it bounds how quickly
/// the thread notices it is no longer wanted.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// How long to wait for the camera to pick up.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Nobody has collected a frame for this long, so the pipeline is stopped and
/// the camera need not be held open.
pub const IDLE_STOP: Duration = Duration::from_secs(3);

/// How long to wait before dialling again, doubling up to the second.
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Connecting,
    Playing,
    Retrying,
    Failed,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Playing => "playing",
            Self::Retrying => "retrying",
            Self::Failed => "failed",
        }
    }
}

/// One frame of video, at the time it was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    /// Microseconds from the first frame of the connection.
    pub micros: u64,

    /// Microseconds to the frame before this one, zero for the first.
    pub gap: u32,

    pub is_sync: bool,
    pub bytes: Vec<u8>,
}

/// What the thread hands over, in the order it happened.
#[derive(Debug)]
pub enum Item {
    /// The codec and the parameter sets for the frames that follow.
    Codec {
        codec: &'static str,
        extras: Vec<u8>,
    },

    Frame(Media),

    /// The stream stopped. Carries why, which may be nothing to report.
    Ended(String),
}

#[derive(Debug, Clone)]
pub struct Options {
    pub url: String,
    pub user: String,
    pub password: String,
    pub reconnect: bool,

    /// Silence from the camera this long means the connection is gone.
    pub timeout: Duration,

    /// Frames held for the pipeline before the oldest are let go.
    pub queue: usize,

    /// How long nobody collecting means the pipeline has stopped.
    pub idle: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            url: String::new(),
            user: String::new(),
            password: String::new(),
            reconnect: true,
            timeout: Duration::from_secs(10),
            queue: 120,
            idle: IDLE_STOP,
        }
    }
}

/// What the two threads share: a queue one way, readings the other.
pub struct Feed {
    items: Mutex<VecDeque<Item>>,
    said: Mutex<(State, String)>,

    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub dropped: AtomicU64,
    pub packets: AtomicU64,

    /// Milliseconds, counted from `since`, of the last collection and of the
    /// request to stop.
    pulled: AtomicU64,
    stopping: AtomicBool,

    since: Instant,
    queue: usize,
    idle: Duration,
}

impl Feed {
    fn new(options: &Options) -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
            said: Mutex::new((State::Idle, String::new())),
            frames: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            pulled: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
            since: Instant::now(),
            queue: options.queue.max(1),
            idle: options.idle,
        }
    }

    /// The oldest frame goes when the queue is full: a live stream carries on
    /// whether or not the pipeline keeps up, and the newest is what matters.
    fn push(&self, item: Item) {
        let mut items = self.items.lock().expect("the queue");

        while items.len() >= self.queue {
            let oldest = items.iter().position(|held| matches!(held, Item::Frame(_)));

            match oldest {
                Some(at) => {
                    items.remove(at);
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }

                // Nothing but news in there, which is not worth dropping.
                None => break,
            }
        }

        items.push_back(item);
    }

    /// The next thing the pipeline has not seen, and a note that it asked.
    pub fn pop(&self) -> Option<Item> {
        self.collected();

        self.items.lock().expect("the queue").pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.items.lock().expect("the queue").is_empty()
    }

    pub fn state(&self) -> (State, String) {
        self.said.lock().expect("the state").clone()
    }

    fn say(&self, state: State, note: &str) {
        *self.said.lock().expect("the state") = (state, note.to_string());
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    /// Whether the pipeline has stopped collecting, so the camera can be let
    /// go until it starts again.
    fn abandoned(&self) -> bool {
        let pulled = self.pulled.load(Ordering::Relaxed);
        let now = self.since.elapsed().as_millis() as u64;

        now.saturating_sub(pulled) > self.idle.as_millis() as u64
    }

    /// Lets a stream that has just been dialled be collected from, so the
    /// first look does not read as having been abandoned.
    fn collected(&self) {
        self.pulled
            .store(self.since.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

/// The logger, over on the thread that reads the network.
///
/// Sound because the host's log hook is the only thing it calls, and that one
/// is a thread safe sink; nothing else on the context may be touched here.
struct Log(Logger);

unsafe impl Send for Log {}

impl Deref for Log {
    type Target = Logger;

    fn deref(&self) -> &Logger {
        &self.0
    }
}

/// A stream being carried, and the thread carrying it.
pub struct Stream {
    pub feed: Arc<Feed>,
    carrying: Option<JoinHandle<()>>,
}

impl Stream {
    pub fn dial(options: Options, log: Logger) -> Self {
        let feed = Arc::new(Feed::new(&options));

        // Dialled now, so a first look for frames is not taken for a pipeline
        // that never asked.
        feed.collected();

        let carrying = {
            let feed = Arc::clone(&feed);
            let log = Log(log);

            thread::spawn(move || carry(&feed, &options, &log))
        };

        Self {
            feed,
            carrying: Some(carrying),
        }
    }

    /// Whether the thread has given up, so the plugin knows to dial again.
    pub fn finished(&self) -> bool {
        self.carrying
            .as_ref()
            .map(|carrying| carrying.is_finished())
            .unwrap_or(true)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.feed.stop();

        // The thread looks around every read timeout, so this is a wait of
        // half a second at worst.
        if let Some(carrying) = self.carrying.take() {
            let _ = carrying.join();
        }
    }
}

/// Dials, carries, and dials again for as long as it is wanted.
fn carry(feed: &Arc<Feed>, options: &Options, log: &Log) {
    let mut backoff = FIRST_BACKOFF;
    let mut note = String::new();

    while !feed.stopping() {
        feed.say(State::Connecting, "");

        match play(feed, options, log) {
            // Asked to stop, or nobody is collecting any more.
            Ok(()) => {
                note.clear();
                break;
            }

            Err(error) => {
                note = error.to_string();

                log_warn!(log, "{}: {note}", options.url);
            }
        }

        if !options.reconnect || feed.stopping() {
            feed.say(State::Failed, &note);
            break;
        }

        feed.say(State::Retrying, &note);

        if !wait(feed, backoff) {
            break;
        }

        backoff = (backoff * 2).min(MAX_BACKOFF);
    }

    if !matches!(feed.state().0, State::Failed) {
        feed.say(State::Idle, &note);
    }

    feed.push(Item::Ended(note));
}

/// One connection, from dialling to the stream stopping.
fn play(feed: &Arc<Feed>, options: &Options, log: &Log) -> rtsp::Result<()> {
    let Some(url) = url::parse(&options.url) else {
        return Err(rtsp::Error::Protocol(format!(
            "'{}' is not an rtsp address",
            options.url
        )));
    };

    // The fields win over a login written into the address.
    let user = some(&options.user).or_else(|| url.user.clone());
    let password = some(&options.password).or_else(|| url.password.clone());

    let credentials = user.map(|user| Credentials {
        user,
        password: password.unwrap_or_default(),
    });

    let mut session = Session::connect(&url, credentials, DIAL_TIMEOUT, READ_TIMEOUT)?;

    log_debug!(log, "connected to {}", url.host);

    let description = session.describe()?;

    let Some(video) = description.video().cloned() else {
        return Err(rtsp::Error::Protocol(
            "the camera offers no video track".to_string(),
        ));
    };

    let Some((codec, mut depacket)) = rtp::depacketizer(&video.encoding, video.parameter_sets())
    else {
        return Err(rtsp::Error::Protocol(format!(
            "{} is not a codec this plugin carries",
            video.encoding
        )));
    };

    session.setup(&url.control_uri(&video.control))?;
    session.play()?;

    log_info!(
        log,
        "playing {} as {codec} at {} hz",
        url.request_uri(),
        clock_of(&video)
    );

    feed.say(State::Playing, "");

    read(
        feed,
        &mut session,
        &mut *depacket,
        options,
        log,
        &video,
        codec,
    )
}

/// The read loop: packets in, frames out, keepalives in between.
fn read(
    feed: &Arc<Feed>,
    session: &mut Session,
    depacket: &mut dyn Depacketizer,
    options: &Options,
    log: &Log,
    video: &mstu_media::sdp::Media,
    codec: &'static str,
) -> rtsp::Result<()> {
    let clock = clock_of(video);

    let mut heard = Instant::now();
    let mut said = Instant::now();
    let mut clocked = Clock::new(clock);

    loop {
        if feed.stopping() {
            session.teardown();
            return Ok(());
        }

        // The pipeline stopped asking, so the camera is let go until it does
        // again rather than streaming into a queue nobody empties.
        if feed.abandoned() {
            log_debug!(log, "nothing is collecting frames, letting the camera go");

            session.teardown();
            return Ok(());
        }

        match session.receive() {
            Ok(Received::Data { channel, bytes }) if channel == DATA_CHANNEL => {
                heard = Instant::now();
                feed.packets.fetch_add(1, Ordering::Relaxed);

                if let Some(packet) = rtp::parse(&bytes) {
                    // Another track's packets would be read as this one's.
                    if packet.payload_type == video.payload_type {
                        depacket.push(packet.payload, packet.timestamp, packet.marker);

                        if let Some(extras) = depacket.take_parameters() {
                            log_info!(log, "{} byte(s) of parameter sets", extras.len());

                            feed.push(Item::Codec { codec, extras });
                        }

                        while let Some(frame) = depacket.pop() {
                            let (micros, gap) = clocked.at(frame.timestamp);

                            feed.frames.fetch_add(1, Ordering::Relaxed);
                            feed.bytes
                                .fetch_add(frame.bytes.len() as u64, Ordering::Relaxed);

                            feed.push(Item::Frame(Media {
                                micros,
                                gap,
                                is_sync: frame.is_sync,
                                bytes: frame.bytes,
                            }));
                        }
                    }
                }
            }

            // Reports on the other channel, and answers to our keepalives.
            Ok(_) => heard = Instant::now(),

            Err(rtsp::Error::Idle) => {
                if heard.elapsed() > options.timeout {
                    return Err(rtsp::Error::Protocol(format!(
                        "nothing from the camera for {} second(s)",
                        options.timeout.as_secs()
                    )));
                }
            }

            Err(error) => return Err(error),
        }

        // Half the session timeout, so one lost keepalive is not the end.
        if said.elapsed() >= session.timeout / 2 {
            session.keepalive()?;
            said = Instant::now();
        }
    }
}

/// The clock a track's timestamps are on, which video rides at ninety
/// kilohertz unless the description says otherwise.
fn clock_of(video: &mstu_media::sdp::Media) -> u32 {
    match video.clock_rate {
        0 => rtp::VIDEO_CLOCK,
        rate => rate,
    }
}

/// Waits out `how_long`, giving up early if the stream is no longer wanted.
fn wait(feed: &Arc<Feed>, how_long: Duration) -> bool {
    let until = Instant::now() + how_long;

    while Instant::now() < until {
        if feed.stopping() {
            return false;
        }

        thread::sleep(Duration::from_millis(50));
    }

    !feed.stopping()
}

fn some(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_of_nothing_is_the_usual_one() {
        let video = mstu_media::sdp::Media {
            kind: "video".to_string(),
            payload_type: 96,
            encoding: "H264".to_string(),
            clock_rate: 0,
            control: String::new(),
            format: Vec::new(),
        };

        assert_eq!(clock_of(&video), 90_000);
    }

    #[test]
    fn the_queue_lets_the_oldest_frames_go_when_nobody_collects() {
        let feed = Feed::new(&Options {
            queue: 2,
            ..Default::default()
        });

        for at in 0..4u64 {
            feed.push(Item::Frame(Media {
                micros: at,
                gap: 0,
                is_sync: false,
                bytes: vec![],
            }));
        }

        assert_eq!(feed.dropped.load(Ordering::Relaxed), 2);

        match feed.pop() {
            Some(Item::Frame(media)) => assert_eq!(media.micros, 2, "the newest two are kept"),
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    /// News is not a frame, and must survive a queue that is overflowing.
    #[test]
    fn what_the_pipeline_must_hear_is_never_dropped() {
        let feed = Feed::new(&Options {
            queue: 1,
            ..Default::default()
        });

        feed.push(Item::Codec {
            codec: "h264",
            extras: vec![1, 2, 3],
        });

        for at in 0..4u64 {
            feed.push(Item::Frame(Media {
                micros: at,
                gap: 0,
                is_sync: false,
                bytes: vec![],
            }));
        }

        assert!(matches!(feed.pop(), Some(Item::Codec { .. })));
    }

    /// A pipeline that has stopped is a pipeline that stops asking, which is
    /// the only sign this side gets of it.
    #[test]
    fn a_feed_nobody_collects_from_is_abandoned() {
        let mut feed = Feed::new(&Options::default());

        // A feed as it would be a while after the last collection.
        feed.since = Instant::now()
            .checked_sub(IDLE_STOP * 2)
            .expect("a clock with some history");

        feed.pulled.store(0, Ordering::Relaxed);
        assert!(feed.abandoned());

        feed.collected();
        assert!(!feed.abandoned(), "and no longer, once it asks again");
    }
}
