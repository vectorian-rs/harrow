#![no_main]
use libfuzzer_sys::fuzz_target;

use bytes::BytesMut;
use harrow_codec_h1::{PayloadDecoder, PayloadItem};

fuzz_target!(|data: &[u8]| {
    // Fuzz the stateful chunked decoder with incremental feeding.
    // Split the input at an arbitrary point to simulate partial recv.
    for split_at in [0, 1, data.len() / 2, data.len()] {
        let split_at = split_at.min(data.len());
        let mut dec = PayloadDecoder::chunked();
        let mut buf = BytesMut::from(&data[..split_at]);

        // Feed first part.
        loop {
            match dec.decode(&mut buf, Some(1024 * 1024)) {
                Ok(Some(PayloadItem::Eof)) => break,
                Ok(Some(PayloadItem::Chunk(_))) => continue,
                Ok(None) => break,
                Err(_) => break,
            }
        }

        // Feed second part (if not already done or errored).
        if !dec.is_eof() {
            buf.extend_from_slice(&data[split_at..]);
            loop {
                match dec.decode(&mut buf, Some(1024 * 1024)) {
                    Ok(Some(PayloadItem::Eof)) => break,
                    Ok(Some(PayloadItem::Chunk(_))) => continue,
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        }
    }

    // Also fuzz the Content-Length decoder.
    if data.len() >= 2 {
        let len = u16::from_le_bytes([data[0], data[1]]) as u64;
        let mut dec = PayloadDecoder::length(len);
        let mut buf = BytesMut::from(&data[2..]);
        loop {
            match dec.decode(&mut buf, None) {
                Ok(Some(PayloadItem::Eof)) => break,
                Ok(Some(PayloadItem::Chunk(_))) => continue,
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }
});
