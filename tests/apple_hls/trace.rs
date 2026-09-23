//! Optional request evidence for diagnosing validator fetches against a moving live edge.
use std::{fs::File, io::Write, sync::Arc, time::Instant};

use parking_lot::Mutex;
use rushls::{
    delivery::{Body, DeliveryError, DeliveryFailure, Response, Reuse},
    server::http::Application,
};

pub struct Traced<P> {
    application: Arc<P>,
    output: Option<Mutex<File>>,
    started: Instant,
}

impl<P> Traced<P> {
    pub fn new(application: Arc<P>, name: &str) -> std::io::Result<Self> {
        let output = std::env::var_os("RUSHLS_TEST_TRACE_DIR")
            .map(|directory| {
                std::fs::create_dir_all(&directory)?;
                File::create(
                    std::path::PathBuf::from(directory).join(format!("{name}.requests.jsonl")),
                )
                .map(Mutex::new)
            })
            .transpose()?;
        Ok(Self {
            application,
            output,
            started: Instant::now(),
        })
    }
}

impl<P: Application> Application for Traced<P> {
    fn http_meters(&self, path: &str) -> Option<rushls::observe::http::HttpMeters> {
        self.application.http_meters(path)
    }

    async fn serve<'a>(
        &'a self,
        path: &'a str,
        query: Option<&'a str>,
    ) -> Result<Response, DeliveryFailure> {
        let requested = self.started.elapsed().as_secs_f64();
        let response = self.application.serve(path, query).await;
        if let Some(output) = &self.output {
            let tags = match &response {
                Ok(Response {
                    body: Body::Manifest(bytes),
                    ..
                }) => String::from_utf8_lossy(bytes)
                    .lines()
                    .filter(|line| {
                        line.starts_with("#EXT-X-PART:")
                            || line.starts_with("#EXT-X-MEDIA-SEQUENCE:")
                    })
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let entry = serde_json::json!({
                "requested": requested,
                "answered": self.started.elapsed().as_secs_f64(),
                "path": path,
                "blocking": query.is_some_and(|query| query.contains("_HLS_msn=")),
                "max_age": response.as_ref().ok().map(|response| response.reuse.max_age.as_secs_f64()),
                "error": response.as_ref().err().map(|failure| failure.error.to_string()),
                "tags": tags,
            });
            writeln!(output.lock(), "{entry}").map_err(|_| DeliveryFailure {
                error: DeliveryError::Projection,
                reuse: Reuse::revalidate(),
            })?;
        }
        response
    }
}
