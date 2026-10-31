use mstu_sdk::{
    CommandDescriptor, Data, EnumSchema, EnumVariant, EventDescriptor, Field, Schema, Slice, Str,
    TYPE_BOOL, TYPE_BYTES, TYPE_I32, TYPE_STRING, TYPE_U32, TYPE_U64, Type, TypeKind::TypeEnum,
    TypeSchema, Value, ValueKind, slice,
};

pub static SETTINGS_SCHEMA_FIELDS: [Field; 5] = [
    Field {
        name: Str::from_static("url"),
        ty: &TYPE_STRING,
        description: Str::from_static(
            "Address of the stream, as rtsp://host:554/path. A login written into it \
             is used unless the fields below are filled in.",
        ),
    },
    Field {
        name: Str::from_static("username"),
        ty: &TYPE_STRING,
        description: Str::from_static("Login for a camera that asks for one."),
    },
    Field {
        name: Str::from_static("password"),
        ty: &TYPE_STRING,
        description: Str::from_static("Password for that login."),
    },
    Field {
        name: Str::from_static("reconnect"),
        ty: &TYPE_BOOL,
        description: Str::from_static(
            "Dial again when the stream drops, waiting a little longer after each failure.",
        ),
    },
    Field {
        name: Str::from_static("timeout"),
        ty: &TYPE_U32,
        description: Str::from_static(
            "Seconds of silence from the camera before the connection is given up on.",
        ),
    },
];

pub static SETTINGS_SCHEMA: Schema = Schema {
    fields: slice!(SETTINGS_SCHEMA_FIELDS),
};

pub static LIVE_SCHEMA_FIELDS: [Field; 6] = [
    Field {
        name: Str::from_static("state"),
        ty: &TYPE_STRING,
        description: Str::from_static(
            "What the connection is doing: idle, connecting, playing, retrying or failed.",
        ),
    },
    Field {
        name: Str::from_static("frames"),
        ty: &TYPE_U64,
        description: Str::from_static("Frames carried since the stream was dialled."),
    },
    Field {
        name: Str::from_static("bytes"),
        ty: &TYPE_U64,
        description: Str::from_static("Bytes of video carried since the stream was dialled."),
    },
    Field {
        name: Str::from_static("dropped"),
        ty: &TYPE_U64,
        description: Str::from_static(
            "Frames thrown away because the pipeline was not taking them fast enough.",
        ),
    },
    Field {
        name: Str::from_static("bitrate"),
        ty: &TYPE_U64,
        description: Str::from_static("What the stream is carrying, in kilobits per second."),
    },
    Field {
        name: Str::from_static("note"),
        ty: &TYPE_STRING,
        description: Str::from_static(
            "Why the connection is in the state it is, when it is worth saying.",
        ),
    },
];

pub static LIVE_SCHEMA: Schema = Schema {
    fields: slice!(LIVE_SCHEMA_FIELDS),
};

pub static VIDEO_OUTPUT_SCHEMA_FIELDS: [Field; 5] = [
    Field {
        name: Str::from_static("start_time"),
        ty: &TYPE_U64,
        description: Str::from_static(
            "Time of the frame in microseconds, counted from the first one received.",
        ),
    },
    Field {
        name: Str::from_static("duration"),
        ty: &TYPE_U32,
        description: Str::from_static(
            "Microseconds this frame covers, taken from the gap to the frame before it: \
             a live stream does not say in advance, and the first frame has no gap.",
        ),
    },
    Field {
        name: Str::from_static("rendering_offset"),
        ty: &TYPE_I32,
        description: Str::from_static("Always zero: RTP carries no composition time of its own."),
    },
    Field {
        name: Str::from_static("is_sync"),
        ty: &TYPE_BOOL,
        description: Str::from_static("Whether a decoder can start at this frame."),
    },
    Field {
        name: Str::from_static("bytes"),
        ty: &TYPE_BYTES,
        description: Str::from_static("Encoded video frame, Annex-B start code prefixed."),
    },
];

pub static VIDEO_OUTPUT_SCHEMA: Schema = Schema {
    fields: slice!(VIDEO_OUTPUT_SCHEMA_FIELDS),
};

pub static CODEC_ENUM_VARIANTS: [EnumVariant; 2] = [
    EnumVariant {
        name: Str::from_static("H264"),
        value: Value {
            kind: ValueKind::ValueString,
            data: Data {
                string_: Str::from_static("h264"),
            },
        },
    },
    EnumVariant {
        name: Str::from_static("H265"),
        value: Value {
            kind: ValueKind::ValueString,
            data: Data {
                string_: Str::from_static("h265"),
            },
        },
    },
];

pub static CODEC_ENUM_SCHEMA: EnumSchema = EnumSchema {
    variants: slice!(CODEC_ENUM_VARIANTS),
};

pub static CODEC_ENUM_TYPE: Type = Type {
    kind: TypeEnum,
    schema: TypeSchema {
        enum_: &CODEC_ENUM_SCHEMA,
    },
};

/// The same three fields, in the same order, as the file source's
/// `file.changed`, so one wiring downstream serves either source.
pub static STREAM_CHANGED_SCHEMA_FIELDS: [Field; 3] = [
    Field {
        name: Str::from_static("stream_name"),
        ty: &TYPE_STRING,
        description: Str::from_static("Address of the stream that is now playing."),
    },
    Field {
        name: Str::from_static("codec"),
        ty: &CODEC_ENUM_TYPE,
        description: Str::from_static("Codec the stream carries."),
    },
    Field {
        name: Str::from_static("extras"),
        ty: &TYPE_BYTES,
        description: Str::from_static(
            "Parameter sets a decoder needs before the first frame, Annex-B start code prefixed.",
        ),
    },
];

pub static STREAM_CHANGED_SCHEMA: Schema = Schema {
    fields: slice!(STREAM_CHANGED_SCHEMA_FIELDS),
};

pub static STREAM_ENDED_SCHEMA: Schema = Schema {
    fields: Slice::empty(),
};

pub static EVENTS: [EventDescriptor; 2] = [
    EventDescriptor {
        event_name: Str::from_static("stream.changed"),
        schema: &STREAM_CHANGED_SCHEMA,
        description: Str::from_static(
            "Emitted when a stream starts playing, and again if the camera changes \
             its parameter sets mid-stream.",
        ),
    },
    EventDescriptor {
        event_name: Str::from_static("stream.ended"),
        schema: &STREAM_ENDED_SCHEMA,
        description: Str::from_static("Emitted when the stream stops, however it stopped."),
    },
];

pub static RECONNECT_COMMAND_SCHEMA: Schema = Schema {
    fields: Slice::empty(),
};

pub static COMMANDS: [CommandDescriptor; 1] = [CommandDescriptor {
    command_name: Str::from_static("stream.reconnect"),
    schema: &RECONNECT_COMMAND_SCHEMA,
    description: Str::from_static("Drops the connection and dials the camera again."),
}];
