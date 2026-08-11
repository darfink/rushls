//! What FFmpeg this process is actually running against.
//!
//! The build already refuses an FFmpeg older than 8 through `pkg-config`, but
//! that only describes the machine the binary was *compiled* on. FFmpeg is
//! linked dynamically, so the libraries loaded at startup are whatever the
//! runtime environment supplies — a different image layer, an `LD_LIBRARY_PATH`
//! that resolves elsewhere, or a distribution package installed beside the one
//! that was built against.
//!
//! A mismatch matters here because the failure is silent. This origin depends
//! on demuxer behaviour rather than on API surface: FLV script-data captions
//! reaching a subtitle stream, and Enhanced RTMP multitrack messages producing
//! one track per stream. An older library exports the same symbols and links
//! successfully; it simply yields fewer tracks, which surfaces as a publisher
//! whose captions never appear rather than as an error anyone can act on.

use std::fmt;

use derive_more::Display;
use ffmpeg_sys_next as ffmpeg;

/// The oldest major versions this origin's demuxing assumptions hold for.
///
/// These are library sonames rather than FFmpeg release numbers: FFmpeg 8.0
/// ships libavformat 62, libavcodec 62, and libavutil 60. The major component
/// is the whole check — FFmpeg bumps it on every ABI break, and a minor
/// difference within one major is by definition compatible.
const REQUIRED_AVFORMAT_MAJOR: u32 = 62;
const REQUIRED_AVCODEC_MAJOR: u32 = 62;
const REQUIRED_AVUTIL_MAJOR: u32 = 60;

/// One library's version, as FFmpeg's packed `MAJOR.MINOR.MICRO` integer.
///
/// `Debug` renders the components rather than the packed integer: this reaches
/// an operator through `Termination`, and a bare `4066406` explains nothing.
#[derive(Clone, Copy, Display, Eq, PartialEq)]
#[display("{}.{}.{}", self.major(), self.minor(), self.micro())]
pub struct LibraryVersion(u32);

impl fmt::Debug for LibraryVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl LibraryVersion {
    pub fn major(self) -> u32 {
        self.0 >> 16
    }

    pub fn minor(self) -> u32 {
        (self.0 >> 8) & 0xFF
    }

    pub fn micro(self) -> u32 {
        self.0 & 0xFF
    }
}

/// The FFmpeg libraries this process resolved at load time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkedVersions {
    pub avformat: LibraryVersion,
    pub avcodec: LibraryVersion,
    pub avutil: LibraryVersion,
}

impl fmt::Display for LinkedVersions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "libavformat {}, libavcodec {}, libavutil {}",
            self.avformat, self.avcodec, self.avutil
        )
    }
}

/// A library too old for the demuxer behaviour this origin relies on.
///
/// `Debug` defers to `Display` for the same reason: returning this from `main`
/// prints the `Debug` form, and the sentence is the whole point.
#[derive(Clone, Copy, Display, Eq, PartialEq)]
#[display(
    "{library} {found} is older than the required major version {required}; \
     FFmpeg 8 or newer is needed for FLV script-data captions and Enhanced RTMP \
     multitrack ingest"
)]
pub struct VersionMismatch {
    pub library: &'static str,
    pub found: LibraryVersion,
    pub required: u32,
}

impl fmt::Debug for VersionMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl std::error::Error for VersionMismatch {}

/// Reads the versions of the FFmpeg libraries this process is linked against,
/// rejecting any whose major version predates FFmpeg 8.
///
/// Deliberately not comparing against the headers the bindings were generated
/// from. Those are a build-machine fact, and a binary shipped to a host with an
/// older FFmpeg is exactly the case worth catching; the floor is therefore
/// stated here rather than inferred.
pub fn linked_versions() -> Result<LinkedVersions, VersionMismatch> {
    // SAFETY: each of these takes no arguments, touches no state, and returns
    // the packed version constant its library was compiled with.
    let versions = LinkedVersions {
        avformat: LibraryVersion(unsafe { ffmpeg::avformat_version() }),
        avcodec: LibraryVersion(unsafe { ffmpeg::avcodec_version() }),
        avutil: LibraryVersion(unsafe { ffmpeg::avutil_version() }),
    };

    for (library, found, required) in [
        ("libavformat", versions.avformat, REQUIRED_AVFORMAT_MAJOR),
        ("libavcodec", versions.avcodec, REQUIRED_AVCODEC_MAJOR),
        ("libavutil", versions.avutil, REQUIRED_AVUTIL_MAJOR),
    ] {
        if found.major() < required {
            return Err(VersionMismatch {
                library,
                found,
                required,
            });
        }
    }
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linked_libraries_satisfy_the_floor_this_origin_builds_against() {
        // Also asserts the test process itself: a developer whose machine
        // resolves an older FFmpeg would otherwise see subtitle and multitrack
        // tests fail one by one with no shared explanation.
        let versions = linked_versions().expect("the linked FFmpeg is supported");
        assert!(versions.avformat.major() >= REQUIRED_AVFORMAT_MAJOR);
        assert!(versions.avcodec.major() >= REQUIRED_AVCODEC_MAJOR);
        assert!(versions.avutil.major() >= REQUIRED_AVUTIL_MAJOR);
    }

    #[test]
    fn a_packed_version_reads_as_its_three_components() {
        // FFmpeg packs the version as (major << 16) | (minor << 8) | micro.
        let version = LibraryVersion((62 << 16) | (12 << 8) | 102);
        assert_eq!(
            (version.major(), version.minor(), version.micro()),
            (62, 12, 102)
        );
        assert_eq!(version.to_string(), "62.12.102");
    }

    #[test]
    fn a_version_report_names_every_library() {
        let report = linked_versions().expect("the linked FFmpeg is supported");
        let report = report.to_string();
        for library in ["libavformat", "libavcodec", "libavutil"] {
            assert!(report.contains(library), "{report}");
        }
    }

    #[test]
    fn a_mismatch_explains_itself_rather_than_printing_a_packed_integer() {
        // This is what an operator sees when the process refuses to start, and
        // it reaches them through `Debug` because `main` returns it.
        let mismatch = VersionMismatch {
            library: "libavformat",
            found: LibraryVersion((61 << 16) | (7 << 8) | 100),
            required: REQUIRED_AVFORMAT_MAJOR,
        };
        let rendered = format!("{mismatch:?}");
        assert!(rendered.contains("libavformat 61.7.100"), "{rendered}");
        assert!(rendered.contains("FFmpeg 8 or newer"), "{rendered}");
        assert_eq!(rendered, mismatch.to_string());
    }
}
