# mstu-rtsp-source

An [mstu](https://github.com/ng-he/mstu) plugin: video out of an RTSP camera
and into a pipeline.

```sh
./build.sh
./test.sh
```

Which is what the center repository calls to build and test this one.

## What it does

It dials a camera, asks what it has, and carries the video track's frames
downstream as Annex-B, which is the same shape
[mstu-media-file-source](https://github.com/ng-he/mstu-media-file-source)
hands over — so anything already wired for one source takes the other.

| | |
| --- | --- |
| Transport | RTP interleaved in the RTSP connection, over TCP. One socket, nothing for a firewall to drop and no packets to lose. |
| Codecs | H.264 and H.265, single units, aggregation packets and fragments alike. |
| Login | Basic, and digest over MD5, with or without a `qop`. |
| Parameter sets | From SDP where the camera offers them, from the stream where it does not, and again downstream if they change mid-stream. |

UDP, RTCP reports, audio and `rtsps://` are not here. The first version is
what a camera on a network actually needs; the rest can follow.

## Settings

| | |
| --- | --- |
| `url` | `rtsp://host:554/path`. A login written into the address is used unless the fields below are filled in, and never appears downstream or in the logs. |
| `username`, `password` | For a camera that asks. |
| `reconnect` | Dial again when the stream drops, waiting a little longer after each failure, up to ten seconds. |
| `timeout` | Seconds of silence from the camera before the connection is given up on. |

## What comes out

`stream.changed` carries the address, the codec and the parameter sets, in the
same three fields as the file source's `file.changed`, so one subscription
serves either. `stream.ended` says the stream stopped, however it stopped.

Each frame carries its time in microseconds counted from the first frame of
the connection, the gap to the frame before it, whether a decoder can start
there, and the bytes.

## How it is put together

The network is read on a thread of its own, which assembles whole frames and
leaves them in a queue; the engine's thread takes them from there in
`process`. A camera that stalls, drops or needs dialling again never holds the
pipeline up, and nothing on the host is touched from the reading thread.

The queue is bounded: a live stream carries on whether or not the pipeline
keeps up, so the oldest frames are let go and counted as dropped. When nobody
collects for a few seconds — which is what a stopped pipeline looks like from
in here — the camera is let go until it is asked for again.

What is about talking to a camera is here, over the standard library: the
handshake, the interleaved framing, digest authentication and the session
itself. What is about media is in
[mstu-media](https://github.com/ng-he/mstu-media) — RTP packets, the
depacketizers, the clock and SDP — because none of that is RTSP's own and the
packetizer will want the other half of it.

A fake camera in `src/fixture.rs` answers the handshake and sends packets the
media crate builds, so the tests need no network and no camera.
