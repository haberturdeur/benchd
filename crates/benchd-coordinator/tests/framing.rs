//! The protocol switch from line-framed JSON to a raw byte stream.
//!
//! A data channel opens with one JSON line and is opaque bytes thereafter. The
//! switch has to hand over any payload the line codec already buffered — which
//! is easy to get wrong in a way that only a real network exposes.

use futures::StreamExt;
use tokio_util::codec::{FramedRead, LinesCodec};

/// A line codec reads in chunks, so once it has produced a line it may already
/// hold the bytes that follow. `into_inner` discards that buffer; `into_parts`
/// returns it.
///
/// On loopback the hello line and the payload almost always arrive in separate
/// reads and nothing is lost. Across a real network they coalesce into one TCP
/// segment, and the far end then reads a header of zeroes. That is exactly how
/// this was found: every loopback test passed, and the first claim over a real
/// link failed with `usbip version mismatch: peer speaks 0x0000`.
#[tokio::test]
async fn switching_out_of_line_framing_keeps_the_bytes_that_follow() {
    let (mut writer, reader) = tokio::io::duplex(4096);

    // One write, as a single segment would deliver it: the hello line and the
    // first bytes of the binary protocol that follows.
    let payload: &[u8] = &[0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0];
    let mut wire = Vec::new();
    wire.extend_from_slice(br#"{"channel":"k1","side":"host"}"#);
    wire.push(b'\n');
    wire.extend_from_slice(payload);
    tokio::io::AsyncWriteExt::write_all(&mut writer, &wire)
        .await
        .unwrap();

    let mut lines = FramedRead::new(reader, LinesCodec::new());
    let hello = lines.next().await.unwrap().unwrap();
    assert!(hello.contains("\"side\":\"host\""));

    let parts = lines.into_parts();
    assert_eq!(
        &parts.read_buf[..],
        payload,
        "the payload that arrived with the hello must survive the handover; \
         `into_inner` would have dropped it and the peer would read zeroes"
    );
}
