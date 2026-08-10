//! Runs the shipped caption detector over a real captured bitstream.
//!
//! The unit tests build their own SEI, so they can only prove the detector
//! agrees with this author's reading of the spec. This runs the same code over
//! access units produced by an actual encoder and remuxed by FFmpeg, which is
//! what makes the agreement meaningful.
//!
//! Usage: `cargo run --example caption_probe -- <samples.bin> <avcc.bin>`

use rushls::{
    domain::{Codec, DiscoveredTrack, MediaParameters, Payload, Timebase, TrackId},
    source::H264CaptionDetector,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let samples = std::fs::read(args.next().expect("samples path")).expect("read samples");
    let avcc = args
        .next()
        .map(|path| std::fs::read(path).expect("read avcC"))
        .unwrap_or_default();

    let track = DiscoveredTrack {
        id: TrackId(0),
        source_key: None,
        codec: Codec::H264,
        parameters: MediaParameters::Video {
            width: nz::u32!(640),
            height: nz::u32!(480),
            frame_rate: None,
            video_delay: 0,
        },
        timebase: Timebase::new(nz::u32!(1), nz::u32!(90_000)),
        first_pts: None,
        title: None,
        language: None,
        codec_extradata: Payload::from(avcc),
    };

    let mut detector = H264CaptionDetector::new(&track).expect("an H.264 track");
    // The whole mdat is handed over as one access unit: the detector walks NAL
    // framing itself, so this exercises exactly the path a packet would take.
    detector.inspect(&samples);

    let observed = detector.observed();
    println!("a53_present     = {}", observed.a53_present);
    println!("cea608_fields   = {:#04b}", observed.cea608_fields);
    println!("dtvcc_present   = {}", observed.dtvcc_present);
    println!("malformed_sei   = {}", detector.malformed_sei());
}
