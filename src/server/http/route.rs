//! Turning a request path and query into a delivery request.
//!
//! The only genuinely awkward part is that a stream identity is an
//! operator-chosen string that may contain slashes — `live/camera` is an
//! ordinary name — so the path cannot be matched left to right. The resource
//! tail is the fixed-width end, so it is parsed from the right and whatever
//! precedes it is the stream.

use crate::{
    delivery::hls::{
        project::uri::{ResourcePathError, parse_tail, percent_decode},
        serve::{BlockingReload, DeliveryError, Request},
    },
    domain::StreamId,
};

/// The longest path this origin will even attempt to parse.
///
/// A bound here keeps a pathological path from being split into thousands of
/// segments before anything has decided it is nonsense.
const MAXIMUM_PATH_BYTES: usize = 2_048;
const MAXIMUM_PATH_SEGMENTS: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Routed {
    pub stream: StreamId,
    pub request: Request,
}

/// Resolves a request path and query string.
pub fn route(path: &str, query: Option<&str>) -> Result<Routed, DeliveryError> {
    if path.len() > MAXIMUM_PATH_BYTES {
        return Err(DeliveryError::UnknownResource);
    }
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() > MAXIMUM_PATH_SEGMENTS {
        return Err(DeliveryError::UnknownResource);
    }
    // Traversal cannot reach anything — every resource is served from memory by
    // identity, not from a filesystem — but a path containing these is not a
    // name this origin ever produced, so it is a miss rather than a lookup.
    if segments
        .iter()
        .any(|segment| *segment == "." || *segment == "..")
    {
        return Err(DeliveryError::UnknownResource);
    }

    let (resource, consumed) = parse_tail(&segments).map_err(|error| match error {
        ResourcePathError::Unrecognized | ResourcePathError::InvalidIdentifier => {
            DeliveryError::UnknownResource
        }
    })?;
    let stream_segments = &segments[..segments.len() - consumed];
    if stream_segments.is_empty() {
        return Err(DeliveryError::UnknownResource);
    }
    let mut stream = String::new();
    for segment in stream_segments {
        if !stream.is_empty() {
            stream.push('/');
        }
        stream.push_str(&percent_decode(segment).ok_or(DeliveryError::UnknownResource)?);
    }

    let directives = Directives::parse(query)?;
    Ok(Routed {
        stream: StreamId::new(stream),
        request: Request::from_resource(resource, directives.blocking)?,
    })
}

/// The `_HLS_*` delivery directives carried in a query string.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Directives {
    blocking: Option<BlockingReload>,
}

impl Directives {
    fn parse(query: Option<&str>) -> Result<Self, DeliveryError> {
        let Some(query) = query else {
            return Ok(Self::default());
        };
        let mut msn = None;
        let mut part = None;
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "_HLS_msn" => {
                    msn = Some(value.parse::<u64>().map_err(|_| {
                        DeliveryError::InvalidDirective("_HLS_msn must be a number")
                    })?)
                }
                "_HLS_part" => {
                    part = Some(value.parse::<u32>().map_err(|_| {
                        DeliveryError::InvalidDirective("_HLS_part must be a number")
                    })?)
                }
                // An unrecognised directive is not an error: the protocol adds
                // them over time, and refusing one this origin has not learned
                // yet would break a client that is otherwise correct.
                _ => {}
            }
        }
        Ok(Self {
            blocking: BlockingReload::from_directives(msn, part)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        delivery::hls::{Msn, PartId, PartIndex, SegmentId},
        domain::{MediaKind, RenditionId},
    };

    use super::*;

    #[test]
    fn a_stream_name_may_contain_slashes() {
        assert_eq!(
            route("/live/camera/3/video.m3u8", None).expect("routes"),
            Routed {
                stream: StreamId::new("live/camera"),
                request: Request::MediaPlaylist {
                    rendition: RenditionId(3),
                    kind: MediaKind::Video,
                    blocking: None
                }
            }
        );
        assert_eq!(
            route("/a/b/c/d/index.m3u8", None).expect("routes").stream,
            StreamId::new("a/b/c/d")
        );
    }

    #[test]
    fn media_resources_route_by_their_durable_identity() {
        assert_eq!(
            route("/s/0/segment/7.m4s", None).expect("routes").request,
            Request::Segment {
                rendition: RenditionId(0),
                segment: SegmentId(7)
            }
        );
        assert_eq!(
            route("/s/0/part/41.m4s", None).expect("routes").request,
            Request::Part {
                rendition: RenditionId(0),
                part: PartId(41)
            }
        );
    }

    #[test]
    fn a_percent_encoded_stream_decodes_to_one_identity() {
        assert_eq!(
            route("/live%20one/0/video.m3u8", None)
                .expect("routes")
                .stream,
            StreamId::new("live one")
        );
        assert_eq!(
            route("/live%zz/0/video.m3u8", None).unwrap_err(),
            DeliveryError::UnknownResource,
            "an invalid escape would let two spellings name one stream"
        );
    }

    #[test]
    fn blocking_directives_are_read_and_validated() {
        assert_eq!(
            route("/s/0/video.m3u8", Some("_HLS_msn=4&_HLS_part=2"))
                .expect("routes")
                .request,
            Request::MediaPlaylist {
                rendition: RenditionId(0),
                kind: MediaKind::Video,
                blocking: Some(BlockingReload {
                    msn: Msn(4),
                    part: Some(PartIndex(2))
                })
            }
        );
        assert!(matches!(
            route("/s/0/video.m3u8", Some("_HLS_part=2")).unwrap_err(),
            DeliveryError::InvalidDirective(_)
        ));
        assert!(matches!(
            route("/s/0/video.m3u8", Some("_HLS_msn=soon")).unwrap_err(),
            DeliveryError::InvalidDirective(_)
        ));
        assert_eq!(
            route("/s/0/video.m3u8", Some("_HLS_future=1&x=2"))
                .expect("routes")
                .request,
            Request::MediaPlaylist {
                rendition: RenditionId(0),
                kind: MediaKind::Video,
                blocking: None
            },
            "a directive this origin has not learned yet is not a client error"
        );
    }

    #[test]
    fn paths_that_name_nothing_are_misses_rather_than_lookups() {
        for path in [
            "/",
            "/index.m3u8",
            "/s/0/../../etc/passwd",
            "/s/0/segment/7",
            "/s/0/elsewhere/7.m4s",
        ] {
            assert_eq!(
                route(path, None).unwrap_err(),
                DeliveryError::UnknownResource,
                "{path}"
            );
        }
    }
}
