# Players

Rushls serves standard HLS: CMAF (fragmented MP4) media, Low-Latency HLS parts, and WebVTT subtitles.
Any player with LL-HLS support can play it, and players without it play the same playlists with regular segments.

## What is tested

Every change runs these players against live output:

| Player | Coverage |
| --- | --- |
| Safari and Apple's `mediastreamvalidator` / `hlsreport` | Packaging checks and a two-hour live authoring audit |
| Chrome with hls.js 1.7.3 | Live streams, rendition switches, gaps, and the end of the stream, with the correction below |
| GStreamer 1.28 (`hlsdemux2`) | Live playback, decoded audio and video, and end of stream |

## Codecs

H.264 with AAC-LC plays everywhere. The other codecs Rushls packages depend on the player and the platform:
HEVC and AV1 need hardware or software decoders the player can reach, and support for Opus and FLAC in HLS varies.
FLAC output has been checked with decoders, not with browsers or native players.
Test your target players before publishing anything other than H.264 and AAC-LC.

## hls.js 1.7.3

One tested case fails in the official hls.js 1.7.3 release: a rendition switch during live playback,
followed by the end of the stream (`EXT-X-ENDLIST`). The player skips parts it has not loaded yet and stops early.
This happens with valid media, not only with gaps in the input.

The Chrome check uses a [local correction](../tools/patches/hls.js/README.md) of hls.js, which has not been submitted upstream yet.
Release archives do not include a player, so a site that plays Rushls with hls.js uses its own copy.

## Chrome without hls.js

Chrome 154 plays an HLS URL in a plain `<video>` element, but not as Low-Latency HLS.
It downloads whole segments, reloads playlists without blocking, and never requests parts,
so it plays about three segments behind the live edge (18 seconds with the default 6-second segments).

It also fails a live playlist that has fewer than three segments, with `DEMUXER_ERROR_COULD_NOT_PARSE`.
A viewer who opens a stream in its first three segments sees that error; reloading after that works.
The same happens with static files and no Rushls involved, so this is Chrome's behaviour, not a problem with the media.

For low latency, and for playback from the start of a stream, use hls.js in Chrome and other browsers.

## Gaps in the input

With `publish.strict = false`, holes in the input are served as `EXT-X-GAP` (see [Input handling](input-handling.md)).
Players handle them differently:

- Safari continues past audio gaps, and continues video at the next keyframe when audio is present.
  Video-only playback ends early at a video gap.
- hls.js continues past gaps in the tested cases, but some startup and end-of-stream cases with gaps still fail.
- GStreamer plays the uninterrupted stream; its behaviour at a gap is not established.

A gap never restores missing pictures. Players may freeze, skip, or pause briefly around it.

## Authenticated playback

With `[playback.auth]`, a player either sends `Authorization: Bearer` on every request, or passes the token as `?token=`.
The query form needs HLS version 11 `EXT-X-DEFINE:QUERYPARAM` support. hls.js-light does not have it.
See [playback authorization](configuration.md#playback).
