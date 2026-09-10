//! Fuzz the frame decoder.
//!
//! This is the code that consumes untrusted bytes from a USB device. A panic here is a denial
//! of service at minimum, and the decoder's contract is explicit: it must never panic, and it
//! must resynchronise on its own after anything.

#![no_main]

use libfuzzer_sys::fuzz_target;
use wattson_protocol::Decoder;

fuzz_target!(|data: &[u8]| {
    let mut decoder = Decoder::new();

    // Feed in varying chunk sizes: a transport hands over whatever the OS gives it, and frame
    // boundaries never align with read sizes.
    let chunk = 1 + (data.first().copied().unwrap_or(0) as usize % 64);
    for part in data.chunks(chunk) {
        decoder.feed(part, &mut |_seq, frame| {
            // Touch every borrowed payload, so a bad length or offset is actually reached
            // rather than merely constructed.
            match frame {
                wattson_protocol::Frame::CurrentSamples(b) => {
                    for s in b.iter() {
                        std::hint::black_box(s);
                    }
                }
                wattson_protocol::Frame::Event(b) => {
                    for e in b.iter() {
                        std::hint::black_box(e);
                    }
                }
                wattson_protocol::Frame::GpioEvent(b) => {
                    for g in b.iter() {
                        std::hint::black_box(g);
                    }
                }
                other => {
                    std::hint::black_box(other.type_byte());
                }
            }
        });
    }

    std::hint::black_box(decoder.stats());
});
