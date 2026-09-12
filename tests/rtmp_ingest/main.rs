//! Real publishers exercise the socket adapter, including chunk fragmentation.
mod fixtures;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires ffmpeg with RTMP and Enhanced FLV support"]
async fn ffmpeg_h264_aac_retains_every_sample() -> TestResult {
    fixtures::publish_and_compare("h264_aac.flv").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires ffmpeg with RTMP and Enhanced FLV support"]
async fn ffmpeg_hevc_aac_retains_every_sample() -> TestResult {
    fixtures::publish_and_compare("hevc_aac.flv").await
}
