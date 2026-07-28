//! Turning a request path and query into a delivery request.
//!
//! The path itself is resolved by [`uri`](crate::delivery::hls::uri), which owns
//! both directions of the naming scheme. What is left here is HTTP's own half:
//! the `_HLS_*` directives a query string carries, and the mapping from a name
//! this origin never produced onto the error the server layer reports.

use crate::{
    delivery::hls::{
        serve::{BlockingReload, DeliveryError, Request},
        uri::parse_path,
    },
    domain::StreamId,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Routed {
    pub stream: StreamId,
    pub request: Request,
}

/// Resolves a request path and query string.
pub fn route(path: &str, query: Option<&str>) -> Result<Routed, DeliveryError> {
    // Every way a path can fail to name something is the same answer here: this
    // origin did not produce that name.
    let named = parse_path(path).map_err(|_| DeliveryError::UnknownResource)?;
    let directives = Directives::parse(query)?;
    Ok(Routed {
        stream: named.stream,
        request: Request::new(named.resource, directives.blocking),
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
        delivery::hls::{Msn, PartIndex, uri::Resource},
        domain::{MediaKind, RenditionId},
    };

    use super::*;

    /// The resource every directive test below is attached to; which paths name
    /// which resources is [`uri`](crate::delivery::hls::uri)'s own business.
    fn video() -> Resource {
        Resource::MediaPlaylist(RenditionId(0), MediaKind::Video)
    }

    #[test]
    fn a_resolved_path_carries_its_stream_and_resource() {
        assert_eq!(
            route("/live/camera/0/video.m3u8", None).expect("routes"),
            Routed {
                stream: StreamId::new("live/camera"),
                request: Request::new(video(), None)
            }
        );
        assert_eq!(
            route("/live/camera/nowhere.m3u8", None).unwrap_err(),
            DeliveryError::UnknownResource,
            "a name this origin never produced is a miss, whatever made it \
             unparseable"
        );
    }

    #[test]
    fn blocking_directives_are_read_and_validated() {
        assert_eq!(
            route("/s/0/video.m3u8", Some("_HLS_msn=4&_HLS_part=2"))
                .expect("routes")
                .request,
            Request::new(
                video(),
                Some(BlockingReload {
                    msn: Msn(4),
                    part: Some(PartIndex(2))
                })
            )
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
            Request::new(video(), None),
            "a directive this origin has not learned yet is not a client error"
        );
    }
}
