//! A source that carries video out of an RTSP camera.
//!
//! The network is read on a thread of its own, which puts whole frames in a
//! queue; the engine's thread takes them from there, so a camera that stalls
//! or drops never holds the pipeline up. Reconnecting is this plugin's own
//! business and says so through its live readings rather than failing.

use std::ptr;
use std::time::{Duration, Instant};

use mstu_sdk::{
    CommandDescriptor, EventDescriptor, HostContext, Logger, Message, Metadata, PluginDescriptor,
    PluginHandle, ProcessContext, Schema, Slice, Str, Value, Writer, log_debug, log_error,
    log_info, log_warn, message, slice,
};

pub mod md5;
pub mod rtsp;
pub mod schemas;
pub mod stream;
pub mod url;

#[cfg(test)]
pub mod fixture;

use stream::{Item, Media, Options, State, Stream};

static METADATA: Metadata = Metadata {
    name: Str::from_static("RTSP source"),
    version: Str::from_static(env!("CARGO_PKG_VERSION")),
};

/// How often readings go out while nothing in particular is happening.
const LIVE_INTERVAL: Duration = Duration::from_millis(250);

/// The window a bitrate is measured over.
const RATE_WINDOW: Duration = Duration::from_secs(1);

pub struct RtspSourcePlugin {
    id: String,
    ctx: *const HostContext,
    log: Logger,

    options: Options,
    stream: Option<Stream>,

    /// The address with any login taken out of it, which is what goes
    /// downstream and into the logs.
    name: String,

    /// Set when a setting changed or a reconnect was asked for, so the next
    /// round dials again.
    redial: bool,

    bitrate: Rate,

    /// When readings last went out.
    beat: Option<Instant>,
}

impl RtspSourcePlugin {
    fn new(ctx: *const HostContext, id: String) -> Self {
        let log = Logger::new(ctx, unsafe { METADATA.name.as_str() }, id.as_str());

        Self {
            ctx,
            log,
            id,
            options: Options::default(),
            stream: None,
            name: String::new(),
            redial: false,
            bitrate: Rate::default(),
            beat: None,
        }
    }

    /// Pushes what the UI shows; the host keeps the latest.
    fn publish_live(&mut self) {
        let (state, note) = match self.stream.as_ref() {
            Some(stream) => stream.feed.state(),
            None => (State::Idle, String::new()),
        };

        let (frames, bytes, dropped) = match self.stream.as_ref() {
            Some(stream) => {
                let feed = &stream.feed;

                (
                    feed.frames.load(std::sync::atomic::Ordering::Relaxed),
                    feed.bytes.load(std::sync::atomic::Ordering::Relaxed),
                    feed.dropped.load(std::sync::atomic::Ordering::Relaxed),
                )
            }
            None => (0, 0, 0),
        };

        let values = [
            Str::from_static(state.as_str()).into(),
            frames.into(),
            bytes.into(),
            dropped.into(),
            self.bitrate.sample(bytes).into(),
            Str::new(note.as_str()).into(),
        ];

        unsafe {
            ((*self.ctx).publish_live)(
                Str::new(self.id.as_str()),
                Message {
                    values: Slice::from_raw_parts(values.as_ptr(), values.len()),
                },
            );
        }

        self.beat = Some(Instant::now());
    }

    /// Readings on the slow beat, for a page watching a stream that is only
    /// sitting there.
    fn beat(&mut self) {
        let due = match self.beat {
            Some(beat) => beat.elapsed() >= LIVE_INTERVAL,
            None => true,
        };

        if due {
            self.publish_live();
        }
    }

    /// Whether to dial, which is the first round after an address is given
    /// and every round after a stream of ours has given up.
    fn wants_dialling(&self) -> bool {
        if self.options.url.is_empty() {
            return false;
        }

        let Some(stream) = self.stream.as_ref() else {
            return true;
        };

        // Everything it had to say is said before it is replaced, and one
        // that failed for good is not dialled again behind the user's back.
        stream.finished()
            && stream.feed.is_empty()
            && !matches!(stream.feed.state().0, State::Failed)
    }

    fn dial(&mut self) {
        // The old thread goes first, so two are never on one camera.
        self.stream = None;

        self.name = match url::parse(&self.options.url) {
            Some(url) => url.request_uri(),
            None => self.options.url.clone(),
        };

        log_info!(self.log, "dialling {}", self.name);

        let log = Logger::new(
            self.ctx,
            unsafe { METADATA.name.as_str() },
            self.id.as_str(),
        );

        self.stream = Some(Stream::dial(self.options.clone(), log));
        self.bitrate = Rate::default();

        self.publish_live();
    }

    /// Tells downstream what is coming: the codec, and what a decoder needs
    /// in front of the first frame.
    fn changed(&mut self, codec: &'static str, extras: &[u8]) {
        log_info!(
            self.log,
            "{} is {codec}, with {} byte(s) of parameter sets",
            self.name,
            extras.len()
        );

        unsafe {
            // The host hands back nothing if it has no such event to fill,
            // and the stream plays either way.
            let writer = ((*self.ctx).new_event)(Str::new(self.id.as_str()), 0) as *mut Writer;

            if writer.is_null() {
                log_warn!(self.log, "the host took no stream.changed to fill");
            } else {
                let event = &mut *writer;

                let values = [
                    Str::new(self.name.as_str()).into(),
                    Str::from_static(codec).into(),
                    Slice::from(extras).into(),
                ];

                (event.fill)(
                    event,
                    Message {
                        values: Slice::from_raw_parts(values.as_ptr(), values.len()),
                    },
                );

                ((*self.ctx).publish_events)(Str::new(self.id.as_str()), 0);
            }
        }

        self.publish_live();
    }

    /// Writes one frame into the message the engine brought.
    fn carry(&mut self, output: &mut message::Writer, media: &Media) -> Result<(), String> {
        let payload = (output.reserve)(output, media.bytes.len());

        if payload.is_null() {
            return Err("the message would not take the frame".to_string());
        }

        let room = unsafe { std::slice::from_raw_parts_mut(payload, media.bytes.len()) };

        room.copy_from_slice(&media.bytes);

        let bytes = Slice::from_raw_parts(payload, media.bytes.len());

        log_debug!(
            self.log,
            "frame at {} us, {} byte(s){}",
            media.micros,
            media.bytes.len(),
            if media.is_sync { ", a key frame" } else { "" }
        );

        let values = [
            media.micros.into(),
            (media.gap as u64).into(),
            0i64.into(),
            media.is_sync.into(),
            bytes.into(),
        ];

        (output.fill)(
            output,
            Message {
                values: Slice::from_raw_parts(values.as_ptr(), values.len()),
            },
        );

        self.publish_live();

        Ok(())
    }

    /// The stream stopped, however it stopped: say so once.
    fn ended(&mut self, note: &str) {
        if note.is_empty() {
            log_info!(self.log, "{} stopped", self.name);
        } else {
            log_error!(self.log, "{} stopped: {note}", self.name);
        }

        unsafe {
            // stream.ended carries no payload, but it still has to be queued
            // before it can be published.
            ((*self.ctx).new_event)(Str::new(self.id.as_str()), 1);
            ((*self.ctx).publish_events)(Str::new(self.id.as_str()), 1);
        }

        self.publish_live();
    }

    fn process(&mut self, output: &mut message::Writer) -> Result<(), String> {
        if self.redial {
            self.redial = false;
            self.stream = None;
        }

        if self.wants_dialling() {
            self.dial();
        }

        let Some(stream) = self.stream.as_ref() else {
            self.beat();
            return Ok(());
        };

        match stream.feed.pop() {
            Some(Item::Codec { codec, extras }) => self.changed(codec, &extras),
            Some(Item::Frame(media)) => self.carry(output, &media)?,
            Some(Item::Ended(note)) => self.ended(&note),

            // Nothing yet, which is most rounds: the camera sets the pace.
            None => self.beat(),
        }

        Ok(())
    }
}

/// What the stream is carrying, measured over a window rather than counted
/// from the beginning, so it reads as what is happening now.
#[derive(Default)]
struct Rate {
    at: Option<Instant>,
    bytes: u64,
    kbits: u64,
}

impl Rate {
    fn sample(&mut self, bytes: u64) -> u64 {
        let Some(at) = self.at else {
            self.at = Some(Instant::now());
            self.bytes = bytes;

            return 0;
        };

        let elapsed = at.elapsed();

        if elapsed < RATE_WINDOW {
            return self.kbits;
        }

        // Bits over milliseconds is kilobits over seconds.
        let carried = bytes.saturating_sub(self.bytes);

        self.kbits = carried * 8 / elapsed.as_millis().max(1) as u64;
        self.at = Some(Instant::now());
        self.bytes = bytes;

        self.kbits
    }
}

extern "C" fn metadata() -> &'static Metadata {
    &METADATA
}

extern "C" fn create(ctx: *const HostContext, id: Str) -> PluginHandle {
    let mut plugin = RtspSourcePlugin::new(ctx, id.to_string());

    log_info!(plugin.log, "created");

    // So a page opening before anything happens has readings to show.
    plugin.publish_live();

    Box::into_raw(Box::new(plugin)) as PluginHandle
}

extern "C" fn id(instance: PluginHandle) -> Str {
    let plugin = unsafe { &*(instance as *mut RtspSourcePlugin) };
    Str::new(plugin.id.as_str())
}

extern "C" fn release(instance: PluginHandle) {
    unsafe {
        let plugin = Box::from_raw(instance as *mut RtspSourcePlugin);

        log_info!(plugin.log, "released");

        // Dropping the stream stops the thread and lets the camera go.
        drop(plugin);
    }
}

extern "C" fn process(instance: PluginHandle, p_ctx: *mut ProcessContext) -> bool {
    let plugin = unsafe { &mut *(instance as *mut RtspSourcePlugin) };

    match plugin.process(unsafe { &mut *(*p_ctx).output }) {
        Ok(()) => true,
        Err(err) => {
            log_error!(plugin.log, "{err}");
            false
        }
    }
}

extern "C" fn process_input_schema() -> *const Schema {
    ptr::null()
}

extern "C" fn process_output_schema() -> *const Schema {
    &schemas::VIDEO_OUTPUT_SCHEMA
}

extern "C" fn settings_schema() -> *const Schema {
    &schemas::SETTINGS_SCHEMA
}

extern "C" fn settings(_instance: PluginHandle, _output: *mut message::Writer) -> bool {
    true
}

extern "C" fn set_parameter(instance: PluginHandle, field: usize, value: Value) -> bool {
    let plugin = unsafe { &mut *(instance as *mut RtspSourcePlugin) };

    // Nothing is dialled here: the settings arrive one by one while the
    // pipeline is stopped, and dialling on each would mean a connection per
    // field. The next round of `process` picks the change up.
    match field {
        0 => {
            let Some(url) = value.get::<Str>() else {
                log_error!(plugin.log, "parameter 0 (url) must be a string");
                return false;
            };

            plugin.options.url = unsafe { url.as_str() }.trim().to_string();
            plugin.redial = true;

            log_info!(plugin.log, "url {}", plugin.options.url);
        }

        1 => {
            plugin.options.user = text(value);
            plugin.redial = true;
        }

        2 => {
            plugin.options.password = text(value);
            plugin.redial = true;
        }

        3 => {
            plugin.options.reconnect = value.get::<bool>().unwrap_or(true);
            plugin.redial = true;

            log_info!(plugin.log, "reconnect {}", plugin.options.reconnect);
        }

        4 => {
            let seconds = value.get::<u64>().unwrap_or(0).max(1);

            plugin.options.timeout = Duration::from_secs(seconds);
            plugin.redial = true;

            log_info!(plugin.log, "timeout {seconds}s");
        }

        _ => log_warn!(plugin.log, "ignored unknown parameter {field}"),
    }

    true
}

fn text(value: Value) -> String {
    value
        .get::<Str>()
        .map(|text| unsafe { text.as_str() }.to_string())
        .unwrap_or_default()
}

/// UI folder, resolved by the host against the plugin library directory.
extern "C" fn ui() -> Str {
    Str::from_static("mstu-rtsp-source-ui")
}

extern "C" fn live_schema() -> *const Schema {
    &schemas::LIVE_SCHEMA
}

extern "C" fn events() -> Slice<EventDescriptor> {
    slice!(schemas::EVENTS)
}

extern "C" fn commands() -> Slice<CommandDescriptor> {
    slice!(schemas::COMMANDS)
}

extern "C" fn invoke(instance: PluginHandle, command: usize, _input: *const Message) -> bool {
    let plugin = unsafe { &mut *(instance as *mut RtspSourcePlugin) };

    match command {
        0 => {
            log_info!(plugin.log, "asked to dial again");

            plugin.redial = true;
            true
        }

        _ => {
            log_warn!(plugin.log, "no such command {command}");
            false
        }
    }
}

static PLUGIN: PluginDescriptor = PluginDescriptor {
    metadata,
    create,
    id,
    release,

    process_input_schema,
    process_output_schema,
    settings_schema,

    settings,
    set_parameter,

    ui,

    process,

    events,
    commands,
    invoke,

    live_schema,
};

#[unsafe(no_mangle)]
pub extern "C" fn plugin_descriptor() -> *const PluginDescriptor {
    &PLUGIN
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::{cell::RefCell, vec};

    use mstu_sdk::LogLevel;

    use super::*;
    use crate::fixture::{Camera, Demand, PPS, SPS, idr, interleaved, rtp, slice};

    /// A frame as the host caught it: time, gap, whether it starts a
    /// decoder, and the bytes.
    type Carried = (u64, u32, bool, Vec<u8>);

    extern "C" fn log_nothing(_: LogLevel, _: Str, _: Str) {}

    extern "C" fn log_disabled(_: LogLevel) -> bool {
        false
    }

    thread_local! {
        /// What the host was told, as it was told it.
        static CHANGED: RefCell<Vec<(String, String, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
        static PUBLISHED: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
        static LIVE: RefCell<Vec<(String, u64, String)>> = const { RefCell::new(Vec::new()) };

        /// The frames that were written into the engine's memory.
        static FRAMES: RefCell<Vec<Carried>> = const { RefCell::new(Vec::new()) };

        /// Standing in for the engine's own, and outliving the message like it.
        static RESERVED: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };

        static EVENT: RefCell<Writer> = RefCell::new(Writer {
            _engine_data: ptr::null_mut(),
            fill: event_fill,
            reserve: hold,
        });
    }

    /// Memory of the host's, which the message may point at.
    extern "C" fn hold(_w: *mut Writer, len: usize) -> *mut u8 {
        RESERVED.with(|reserved| {
            let mut reserved = reserved.borrow_mut();

            reserved.push(vec![0u8; len]);
            reserved
                .last_mut()
                .expect("what was just put there")
                .as_mut_ptr()
        })
    }

    fn text(values: &[Value], at: usize) -> String {
        values
            .get(at)
            .and_then(|value| value.get::<Str>())
            .map(|text| unsafe { text.as_str() }.to_string())
            .unwrap_or_default()
    }

    fn bytes(values: &[Value], at: usize) -> Vec<u8> {
        values
            .get(at)
            .and_then(|value| value.get::<Slice<u8>>())
            .map(|slice| unsafe { slice.as_slice() }.to_vec())
            .unwrap_or_default()
    }

    extern "C" fn event_fill(_w: *mut Writer, message: Message) -> bool {
        let values = message.values_slice();

        CHANGED.with(|changed| {
            changed
                .borrow_mut()
                .push((text(values, 0), text(values, 1), bytes(values, 2)))
        });

        true
    }

    extern "C" fn out_fill(_w: *mut Writer, message: Message) -> bool {
        let values = message.values_slice();

        let number = |at: usize| {
            values
                .get(at)
                .and_then(|value| value.get::<u64>())
                .unwrap_or(0)
        };

        let is_sync = values
            .get(3)
            .and_then(|value| value.get::<bool>())
            .unwrap_or(false);

        FRAMES.with(|frames| {
            frames
                .borrow_mut()
                .push((number(0), number(1) as u32, is_sync, bytes(values, 4)))
        });

        true
    }

    extern "C" fn new_event(_: Str, event: usize) -> *const message::Writer {
        // Only stream.changed has anything to fill.
        if event != 0 {
            return ptr::null();
        }

        EVENT.with(|event| event.as_ptr() as *const message::Writer)
    }

    extern "C" fn publish_events(_: Str, event: usize) {
        PUBLISHED.with(|published| published.borrow_mut().push(event));
    }

    extern "C" fn publish_live(_: Str, snapshot: Message) {
        let values = snapshot.values_slice();

        let frames = values
            .get(1)
            .and_then(|value| value.get::<u64>())
            .unwrap_or(0);

        LIVE.with(|live| {
            live.borrow_mut()
                .push((text(values, 0), frames, text(values, 5)))
        });
    }

    /// A host that answers every call with nothing.
    ///
    /// Leaked on purpose: the engine's context outlives every plugin, and a
    /// test that panics leaves a stream thread running that would otherwise
    /// log through a context that had gone.
    fn host() -> &'static HostContext {
        Box::leak(Box::new(HostContext {
            logger: log_nothing,
            log_enabled: log_disabled,
            new_event,
            publish_events,
            publish_live,
        }))
    }

    fn writer() -> Writer {
        Writer {
            _engine_data: ptr::null_mut(),
            fill: out_fill,
            reserve: hold,
        }
    }

    fn changed() -> Vec<(String, String, Vec<u8>)> {
        CHANGED.with(|changed| changed.borrow().clone())
    }

    fn frames() -> Vec<Carried> {
        FRAMES.with(|frames| frames.borrow().clone())
    }

    fn states() -> Vec<String> {
        LIVE.with(|live| {
            live.borrow()
                .iter()
                .map(|(state, _, _)| state.clone())
                .collect()
        })
    }

    fn notes() -> Vec<String> {
        LIVE.with(|live| {
            live.borrow()
                .iter()
                .map(|(_, _, note)| note.clone())
                .collect()
        })
    }

    fn published() -> Vec<usize> {
        PUBLISHED.with(|published| published.borrow().clone())
    }

    /// A key frame and a plain one, as a camera sends them: the parameter
    /// sets and the IDR share a timestamp, so they are one access unit.
    fn pictures() -> Vec<Vec<u8>> {
        vec![
            interleaved(0, &rtp(96, 1, 9_000, false, SPS)),
            interleaved(0, &rtp(96, 2, 9_000, false, PPS)),
            interleaved(0, &rtp(96, 3, 9_000, true, &idr(&[1, 2, 3]))),
            interleaved(0, &rtp(96, 4, 12_000, true, &slice(&[4, 5, 6]))),
        ]
    }

    /// Runs the plugin the way the engine runs it, until it has what the test
    /// is waiting for or the time is up.
    fn pump(handle: PluginHandle, within: Duration, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + within;
        let mut out = writer();
        let mut ctx = ProcessContext {
            input: ptr::null(),
            output: &mut out,
        };

        while Instant::now() < deadline {
            assert!(
                process(handle, &mut ctx),
                "a camera's troubles are not the pipeline's"
            );

            if done() {
                return true;
            }

            thread::sleep(Duration::from_millis(2));
        }

        done()
    }

    fn dialled(ctx: &HostContext, url: &str) -> PluginHandle {
        let handle = create(ctx as *const HostContext, Str::from_static("TEST"));

        assert!(set_parameter(handle, 0, Str::new(url).into()));

        handle
    }

    #[test]
    fn a_camera_that_is_dialled_has_its_frames_carried() {
        let camera = Camera::start(Demand::Nothing, pictures());
        let ctx = host();
        let handle = dialled(ctx, &camera.url());

        assert!(
            pump(handle, Duration::from_secs(5), || frames().len() >= 2),
            "two frames should have come through: {:?}",
            states()
        );

        // What a decoder needs, before any of it.
        let (name, codec, extras) = changed().first().cloned().expect("stream.changed");

        assert_eq!(name, camera.url(), "the address, as the camera gave it");
        assert_eq!(codec, "h264");
        assert_eq!(extras[..4], [0, 0, 0, 1]);
        assert_eq!(extras.len(), SPS.len() + PPS.len() + 8);
        assert_eq!(published().first(), Some(&0));

        let carried = frames();

        // The parameter sets and the idr arrived under one timestamp, so they
        // are one frame.
        assert_eq!(carried[0].0, 0, "the first frame is the start of time");
        assert!(carried[0].2, "and a key frame");
        assert_eq!(
            carried[0]
                .3
                .windows(4)
                .filter(|w| *w == [0, 0, 0, 1])
                .count(),
            3
        );

        // Three thousand ticks of a ninety kilohertz clock.
        assert_eq!(carried[1].0, 33_333);
        assert_eq!(carried[1].1, 33_333, "the gap to the frame before");
        assert!(!carried[1].2);
        assert_eq!(carried[1].3, [0, 0, 0, 1, 0x41, 4, 5, 6]);

        assert!(states().contains(&"playing".to_string()), "{:?}", states());

        release(handle);
    }

    #[test]
    fn nothing_is_dialled_without_an_address() {
        let ctx = host();
        let handle = create(ctx, Str::from_static("TEST"));

        assert!(!pump(handle, Duration::from_millis(300), || {
            !changed().is_empty()
        }));

        assert!(changed().is_empty());
        assert!(frames().is_empty());
        assert!(
            states().iter().all(|state| state == "idle"),
            "{:?}",
            states()
        );

        release(handle);
    }

    /// A camera that is not there must not take the pipeline down with it.
    #[test]
    fn a_camera_that_is_not_there_is_reported_and_given_up_on() {
        let ctx = host();
        let handle = create(ctx, Str::from_static("TEST"));

        // Nothing listens on port one, so this is refused at once.
        assert!(set_parameter(handle, 3, false.into()), "reconnect off");
        assert!(set_parameter(
            handle,
            0,
            Str::from_static("rtsp://127.0.0.1:1/x").into()
        ));

        assert!(
            pump(handle, Duration::from_secs(5), || published().contains(&1)),
            "stream.ended should have been published: {:?}",
            states()
        );

        assert!(states().contains(&"failed".to_string()), "{:?}", states());
        assert!(
            notes().iter().any(|note| !note.is_empty()),
            "and it should say why"
        );

        // Given up on means given up on: no dialling behind the user's back.
        let before = frames().len();

        pump(handle, Duration::from_millis(300), || false);

        assert_eq!(frames().len(), before);

        release(handle);
    }

    #[test]
    fn the_reconnect_command_dials_again() {
        let camera = Camera::start(Demand::Nothing, pictures());
        let ctx = host();
        let handle = dialled(ctx, &camera.url());

        assert!(pump(handle, Duration::from_secs(5), || !frames().is_empty()));
        assert!(invoke(handle, 0, ptr::null()));

        assert!(
            pump(handle, Duration::from_secs(5), || camera
                .asked_for("DESCRIBE")
                >= 2),
            "the camera should have been dialled a second time: {:?}",
            camera.asked()
        );

        release(handle);
    }

    #[test]
    fn a_login_in_the_address_is_given_to_a_camera_that_asks() {
        let camera = Camera::start(Demand::Digest, pictures());
        let ctx = host();

        let url = camera.url().replace("rtsp://", "rtsp://admin:1234@");
        let handle = dialled(ctx, &url);

        assert!(
            pump(handle, Duration::from_secs(5), || !frames().is_empty()),
            "frames should come through a login: {:?}",
            notes()
        );

        // The address downstream carries no password.
        let (name, _, _) = changed().first().cloned().expect("stream.changed");

        assert!(!name.contains("1234"), "{name}");

        release(handle);
    }

    /// A pipeline that stops asking for frames is a pipeline that stopped, so
    /// the camera is let go rather than streaming into a queue nobody empties.
    #[test]
    fn a_stream_nobody_collects_from_is_let_go() {
        let camera = Camera::start(Demand::Nothing, pictures());
        let ctx = host();

        let mut plugin = RtspSourcePlugin::new(ctx, "TEST".to_string());

        plugin.options.url = camera.url();
        plugin.options.idle = Duration::from_millis(200);
        plugin.options.reconnect = false;

        let mut out = writer();
        let deadline = Instant::now() + Duration::from_secs(5);

        while Instant::now() < deadline && frames().is_empty() {
            plugin.process(&mut out).expect("a round of the pipeline");
            thread::sleep(Duration::from_millis(2));
        }

        assert!(!frames().is_empty(), "the stream should have started");

        // Nobody asks again, which is what a stopped pipeline looks like.
        assert!(
            camera.waited_for("TEARDOWN", 1, Duration::from_secs(3)),
            "the camera should have been let go: {:?}",
            camera.asked()
        );
    }
}
