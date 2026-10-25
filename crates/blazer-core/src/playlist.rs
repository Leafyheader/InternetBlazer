//! Reads the HLS and DASH playlists that video players fetch, to list a stream's qualities in a
//! moment, where asking yt-dlp takes seconds. [`tracks`] then lists the pieces of the quality
//! picked, which the engine downloads itself (see [`crate::stream`]).
//!
//! An HLS master playlist lists one variant per quality, each with its own playlist of pieces;
//! a DASH manifest lists the picture and the sound separately, which ffmpeg joins.

use std::cmp::Reverse;

use reqwest::Client;
use url::Url;

use crate::{
    error::{Error, Result},
    probe,
    stream::{Container, Key, Piece, Track, TrackKind},
    types::{MediaChoice, MediaFormat, MediaInfo},
};

/// Playlists are text; anything this large isn't one.
const MAX_PLAYLIST: usize = 8 * 1024 * 1024;

/// Whether the URL looks like an HLS or DASH playlist.
pub(crate) fn looks_like_playlist(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".mpd")
}

/// The qualities in the playlist at `url`. Fails with [`Error::NotPlaylist`] when the URL leads to
/// something else.
pub(crate) async fn inspect(
    client: &Client,
    url: &Url,
    headers: &[(String, String)],
    ffmpeg: bool,
) -> Result<MediaInfo> {
    let text = fetch(client, url, headers).await?;
    let title = title_from(url);
    if is_hls(&text) {
        let master = hls::parse(&text);
        hls::check_keys(&text)?;
        // A master playlist doesn't say how long the video is; the best variant's playlist does.
        let (duration, guess) = match master.variants.first() {
            Some(best) => {
                let variant_url = url.join(&best.uri).map_err(|e| Error::Invalid(e.to_string()))?;
                let variant = fetch(client, &variant_url, headers).await?;
                hls::check_keys(&variant)?;
                (hls::duration(&variant), None)
            }
            None => {
                let duration = hls::duration(&text);
                (duration, size_from_first_piece(client, url, &text, headers, duration).await)
            }
        };
        Ok(MediaInfo {
            title,
            site: "HLS".into(),
            duration,
            choices: hls::choices(&master, duration, ffmpeg, guess),
            ffmpeg,
        })
    } else if is_dash(&text) {
        let manifest = dash::parse(&text)?;
        Ok(MediaInfo {
            title,
            site: "DASH".into(),
            duration: manifest.duration,
            choices: dash::choices(&manifest, ffmpeg),
            ffmpeg,
        })
    } else {
        Err(Error::NotPlaylist)
    }
}

/// What to download for `format` from the playlist at `url`: the picture, with its sound in it
/// or as a second track, or the sound alone. Fails with [`Error::NotPlaylist`] for what is left
/// to yt-dlp, such as a live stream.
pub(crate) async fn tracks(
    client: &Client,
    url: &Url,
    headers: &[(String, String)],
    format: MediaFormat,
) -> Result<Vec<Track>> {
    let text = fetch(client, url, headers).await?;
    if !is_hls(&text) {
        if !is_dash(&text) {
            return Err(Error::NotPlaylist);
        }
        let manifest = dash::parse(&text)?;
        if manifest.unsupported {
            return Err(Error::NotPlaylist);
        }
        let mut tracks = Vec::new();
        for (rep, kind) in dash::pick(&manifest, format) {
            tracks.push(dash::track(client, headers, url, &manifest, rep, kind).await?);
        }
        // Only a picture or only a sound: nothing that matches what was picked.
        if tracks.is_empty()
            || (format == MediaFormat::Audio) != (tracks[0].kind == TrackKind::Audio)
        {
            return Err(Error::NotPlaylist);
        }
        return Ok(tracks);
    }
    let master = hls::parse(&text);
    if master.variants.is_empty() {
        // A playlist of pieces: one quality, with the sound in it.
        return Ok(vec![hls::track(url, &text, TrackKind::Video, None)?]);
    }
    hls::check_keys(&text)?;
    let variant = hls::pick(&master, format);
    let mut tracks = Vec::new();
    if let (MediaFormat::Video { .. }, Some(variant)) = (format, variant) {
        let variant_url = url.join(&variant.uri).map_err(|e| Error::Invalid(e.to_string()))?;
        let text = fetch(client, &variant_url, headers).await?;
        tracks.push(hls::track(&variant_url, &text, TrackKind::Video, variant.bandwidth)?);
    }
    match hls::sound_for(&master, variant, format) {
        Some(sound) => {
            let sound_url = url.join(&sound.uri).map_err(|e| Error::Invalid(e.to_string()))?;
            let text = fetch(client, &sound_url, headers).await?;
            tracks.push(hls::track(&sound_url, &text, TrackKind::Audio, None)?);
        }
        // The choices offer the sound alone only when it comes apart.
        None if format == MediaFormat::Audio => return Err(Error::NotPlaylist),
        None => {}
    }
    Ok(tracks)
}

async fn fetch(client: &Client, url: &Url, headers: &[(String, String)]) -> Result<String> {
    let resp = probe::request(client, url.as_str(), headers).send().await?;
    if !resp.status().is_success() {
        return Err(Error::from_response(resp.status(), resp.headers()));
    }
    if resp.content_length().is_some_and(|len| len > MAX_PLAYLIST as u64) {
        return Err(Error::NotPlaylist);
    }
    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_PLAYLIST {
        return Err(Error::NotPlaylist);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn is_hls(text: &str) -> bool {
    text.trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U")
}

fn is_dash(text: &str) -> bool {
    text.contains("<MPD")
}

/// A name to start with, from the playlist's file name; the browser usually knows a better one.
fn title_from(url: &Url) -> String {
    let name = url.path_segments().and_then(|mut s| s.next_back()).unwrap_or_default();
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let stem = percent_encoding::percent_decode_str(stem).decode_utf8_lossy();
    if stem.is_empty() { "video".into() } else { stem.into_owned() }
}

/// A size estimate from the bitrate and the length, as neither format states sizes.
fn estimate(bits_per_second: Option<u64>, duration: Option<f64>) -> Option<u64> {
    let bits = bits_per_second? as f64 * duration?;
    (bits > 0.0).then(|| (bits / 8.0) as u64)
}

/// A playlist of pieces states no bitrate, so its size is guessed from the size of its first
/// piece. Pieces are mostly the same length, and of a similar size for that length.
async fn size_from_first_piece(
    client: &Client,
    url: &Url,
    text: &str,
    headers: &[(String, String)],
    duration: Option<f64>,
) -> Option<u64> {
    let (seconds, uri) = hls::first_piece(text)?;
    let piece = url.join(uri).ok()?;
    let bytes = probe::probe(client, piece.as_str(), headers).await.ok()?.total?;
    Some((bytes as f64 * duration? / seconds) as u64)
}

fn label(height: u32, fps: Option<f64>) -> String {
    match fps.filter(|&fps| fps > 30.0) {
        Some(fps) => format!("{height}p{fps:.0}"),
        None => format!("{height}p"),
    }
}

fn drm() -> Error {
    Error::Invalid(
        "this video is protected with DRM, which stops it from being downloaded or copied".into(),
    )
}

mod hls {
    use super::*;

    #[derive(Debug, Default)]
    pub(super) struct Master {
        /// Best first.
        pub variants: Vec<Variant>,
        /// The sound comes as its own stream, which must be joined to the picture.
        pub separate_audio: bool,
        /// The sound streams of their own, in the order listed.
        pub audio: Vec<Rendition>,
    }

    #[derive(Debug)]
    pub(super) struct Variant {
        pub uri: String,
        pub bandwidth: Option<u64>,
        pub height: Option<u32>,
        pub fps: Option<f64>,
        /// The group of sound streams that goes with it, if its sound is not in its own pieces.
        pub audio: Option<String>,
    }

    #[derive(Debug)]
    pub(super) struct Rendition {
        pub group: String,
        pub uri: String,
        pub default: bool,
    }

    pub(super) fn parse(text: &str) -> Master {
        let mut master = Master::default();
        let mut pending: Option<Vec<(String, String)>> = None;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
                pending = Some(attributes(attrs));
            } else if let Some(attrs) = line.strip_prefix("#EXT-X-MEDIA:") {
                let attrs = attributes(attrs);
                let is_audio = get(&attrs, "TYPE") == Some("AUDIO");
                // Sound without a URI is inside the variants' own pieces.
                if is_audio && let Some(uri) = get(&attrs, "URI") {
                    master.separate_audio = true;
                    master.audio.push(Rendition {
                        group: get(&attrs, "GROUP-ID").unwrap_or_default().to_string(),
                        uri: uri.to_string(),
                        default: get(&attrs, "DEFAULT") == Some("YES"),
                    });
                }
            } else if !line.starts_with('#')
                && let Some(attrs) = pending.take()
            {
                let bandwidth = get(&attrs, "AVERAGE-BANDWIDTH")
                    .or_else(|| get(&attrs, "BANDWIDTH"))
                    .and_then(|b| b.parse().ok());
                let height = get(&attrs, "RESOLUTION")
                    .and_then(|r| r.split_once(['x', 'X']))
                    .and_then(|(_, h)| h.parse().ok());
                let fps = get(&attrs, "FRAME-RATE").and_then(|f| f.parse().ok());
                let audio = get(&attrs, "AUDIO").map(str::to_string);
                master.variants.push(Variant {
                    uri: line.to_string(),
                    bandwidth,
                    height,
                    fps,
                    audio,
                });
            }
        }
        master.variants.sort_by_key(|v| Reverse((v.height, v.bandwidth)));
        master
    }

    /// The variant for `format`: the best up to the height asked for, as the choices offered
    /// it, or the smallest when every one is taller.
    pub(super) fn pick(master: &Master, format: MediaFormat) -> Option<&Variant> {
        match format {
            MediaFormat::Video { max_height: Some(max) } => master
                .variants
                .iter()
                .find(|v| v.height.is_none_or(|h| h <= max))
                .or(master.variants.last()),
            _ => master.variants.first(),
        }
    }

    /// The sound stream to go with `variant`: the group's default, or its first. For the sound
    /// alone, any of them will do when the variant names no group.
    pub(super) fn sound_for<'a>(
        master: &'a Master,
        variant: Option<&Variant>,
        format: MediaFormat,
    ) -> Option<&'a Rendition> {
        let group = variant.and_then(|v| v.audio.as_deref());
        let candidates: Vec<&Rendition> = match group {
            Some(group) => master.audio.iter().filter(|r| r.group == group).collect(),
            None if format == MediaFormat::Audio => master.audio.iter().collect(),
            None => Vec::new(),
        };
        candidates.iter().find(|r| r.default).or(candidates.first()).copied()
    }

    /// The pieces of the media playlist `text` at `url`. Fails with [`Error::NotPlaylist`] for
    /// a live stream, which has no end to download to.
    pub(super) fn track(
        url: &Url,
        text: &str,
        kind: TrackKind,
        bandwidth: Option<u64>,
    ) -> Result<Track> {
        check_keys(text)?;
        let join = |uri: &str| url.join(uri).map_err(|e| Error::Invalid(e.to_string()));
        let mut pieces = Vec::new();
        let mut sequence: u64 = 0;
        let mut key: Option<(Url, Option<[u8; 16]>)> = None;
        let mut seconds = 0.0;
        let mut range: Option<(u64, Option<u64>)> = None;
        // Where the last sub-range of each resource ended, which a range without an offset
        // starts from.
        let mut ends: Vec<(Url, u64)> = Vec::new();
        let mut map: Option<(Url, Option<(u64, u64)>)> = None;
        let mut fmp4 = false;
        let mut ended = false;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(n) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                sequence = n.trim().parse().unwrap_or(0);
            } else if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
                let attrs = attributes(attrs);
                key = match get(&attrs, "METHOD") {
                    Some("AES-128") => {
                        let uri = get(&attrs, "URI").ok_or_else(|| {
                            Error::Invalid("the stream's key has no address".into())
                        })?;
                        Some((join(uri)?, get(&attrs, "IV").map(parse_iv).transpose()?))
                    }
                    _ => None,
                };
            } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
                let attrs = attributes(attrs);
                let Some(uri) = get(&attrs, "URI") else { continue };
                let range = get(&attrs, "BYTERANGE")
                    .and_then(parse_range)
                    .map(|(len, start)| (start.unwrap_or(0), len));
                let found = (join(uri)?, range);
                fmp4 = true;
                // Written once, and again only where the stream changes it.
                if map.as_ref() != Some(&found) {
                    pieces.push(Piece {
                        url: found.0.clone(),
                        range: found.1,
                        key: None,
                        seconds: 0.0,
                    });
                    map = Some(found);
                }
            } else if let Some(info) = line.strip_prefix("#EXTINF:") {
                seconds = info.split(',').next().and_then(|s| s.trim().parse().ok()).unwrap_or(0.0);
            } else if let Some(r) = line.strip_prefix("#EXT-X-BYTERANGE:") {
                range = parse_range(r);
            } else if line == "#EXT-X-ENDLIST" || line == "#EXT-X-PLAYLIST-TYPE:VOD" {
                ended = true;
            } else if !line.starts_with('#') {
                let piece_url = join(line)?;
                let range = range.take().map(|(len, start)| {
                    let start = start.unwrap_or_else(|| {
                        ends.iter().find(|(u, _)| *u == piece_url).map_or(0, |(_, end)| *end)
                    });
                    ends.retain(|(u, _)| *u != piece_url);
                    ends.push((piece_url.clone(), start + len));
                    (start, len)
                });
                let key = key.as_ref().map(|(url, iv)| Key {
                    url: url.clone(),
                    // Without its own IV, a piece uses its sequence number.
                    iv: iv.unwrap_or_else(|| u128::from(sequence).to_be_bytes()),
                });
                pieces.push(Piece { url: piece_url, range, key, seconds });
                sequence += 1;
                seconds = 0.0;
            }
        }
        if !ended || pieces.iter().all(|p| p.seconds == 0.0) {
            return Err(Error::NotPlaylist);
        }
        let container = if fmp4 {
            Container::Mp4
        } else {
            match pieces.first().map(|p| p.url.path().to_ascii_lowercase()) {
                Some(path) if path.ends_with(".aac") => Container::Aac,
                Some(path) if path.ends_with(".mp4") || path.ends_with(".m4s") => Container::Mp4,
                _ => Container::Ts,
            }
        };
        Ok(Track { kind, container, pieces, bandwidth })
    }

    /// `length[@offset]`, as `EXT-X-BYTERANGE` and the `BYTERANGE` attribute write it.
    fn parse_range(text: &str) -> Option<(u64, Option<u64>)> {
        let text = text.trim();
        match text.split_once('@') {
            Some((len, start)) => Some((len.parse().ok()?, Some(start.parse().ok()?))),
            None => Some((text.parse().ok()?, None)),
        }
    }

    /// `0x` and 32 hex digits.
    fn parse_iv(text: &str) -> Result<[u8; 16]> {
        let hex = text.trim_start_matches("0x").trim_start_matches("0X");
        let value = u128::from_str_radix(hex, 16)
            .map_err(|_| Error::Invalid(format!("the stream's key has a bad IV: {text}")))?;
        Ok(value.to_be_bytes())
    }

    /// Fails for keys other than plain AES-128, which DRM systems use.
    pub(super) fn check_keys(text: &str) -> Result<()> {
        for line in text.lines() {
            let Some(attrs) = line
                .trim()
                .strip_prefix("#EXT-X-KEY:")
                .or_else(|| line.trim().strip_prefix("#EXT-X-SESSION-KEY:"))
            else {
                continue;
            };
            let attrs = attributes(attrs);
            let method = get(&attrs, "METHOD").unwrap_or("NONE");
            let format = get(&attrs, "KEYFORMAT").unwrap_or("identity");
            if !matches!(method, "NONE" | "AES-128") || format != "identity" {
                return Err(drm());
            }
        }
        Ok(())
    }

    /// The sum of a media playlist's pieces, in seconds.
    pub(super) fn duration(text: &str) -> Option<f64> {
        let total: f64 = text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("#EXTINF:"))
            .filter_map(|l| l.split(',').next()?.trim().parse::<f64>().ok())
            .sum();
        (total > 0.0).then_some(total)
    }

    /// The length and address of a media playlist's first piece.
    pub(super) fn first_piece(text: &str) -> Option<(f64, &str)> {
        let mut seconds = None;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(info) = line.strip_prefix("#EXTINF:") {
                seconds = info.split(',').next()?.trim().parse::<f64>().ok();
            } else if !line.starts_with('#') {
                return seconds.filter(|&s| s > 0.0).map(|s| (s, line));
            }
        }
        None
    }

    /// `guess` is the size to show when the playlist states no bitrate.
    pub(super) fn choices(
        master: &Master,
        duration: Option<f64>,
        ffmpeg: bool,
        guess: Option<u64>,
    ) -> Vec<MediaChoice> {
        let mut choices = Vec::new();
        // Picture and separate sound need ffmpeg to be joined.
        if ffmpeg || !master.separate_audio {
            let mut heights = Vec::new();
            for v in &master.variants {
                let Some(height) = v.height else { continue };
                // The first of each height has the highest bitrate.
                if heights.contains(&height) {
                    continue;
                }
                heights.push(height);
                choices.push(MediaChoice {
                    label: label(height, v.fps),
                    format: MediaFormat::Video { max_height: Some(height) },
                    ext: "mp4".into(),
                    size: estimate(v.bandwidth, duration),
                });
            }
            if choices.is_empty() {
                // A single variant, or variants that don't say their size.
                choices.push(MediaChoice {
                    label: "Best quality".into(),
                    format: MediaFormat::Video { max_height: None },
                    ext: "mp4".into(),
                    size: estimate(master.variants.first().and_then(|v| v.bandwidth), duration)
                        .or(guess),
                });
            }
        }
        if master.separate_audio {
            choices.push(MediaChoice {
                label: "Audio only".into(),
                format: MediaFormat::Audio,
                ext: "m4a".into(),
                size: None,
            });
        }
        choices
    }

    /// `KEY=value,KEY="quoted, value"`, as HLS writes attributes.
    fn attributes(text: &str) -> Vec<(String, String)> {
        let mut attrs = Vec::new();
        let mut rest = text;
        while let Some((key, after)) = rest.split_once('=') {
            let key = key.trim().trim_start_matches(',').trim().to_string();
            let (value, next) = match after.strip_prefix('"') {
                Some(quoted) => match quoted.split_once('"') {
                    Some((value, next)) => (value, next),
                    None => (quoted, ""),
                },
                None => after.split_once(',').map_or((after, ""), |(v, n)| (v, n)),
            };
            attrs.push((key, value.trim().to_string()));
            rest = next.trim_start_matches(',');
        }
        attrs
    }

    fn get<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

mod dash;

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"
360/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,AVERAGE-BANDWIDTH=4000000,RESOLUTION=1920x1080,FRAME-RATE=60.000
1080/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720
720/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720
720b/index.m3u8
";

    #[test]
    fn hls_lists_one_choice_per_height_best_first() {
        let master = hls::parse(MASTER);
        assert!(!master.separate_audio);
        assert_eq!(master.variants[0].uri, "1080/index.m3u8");
        let choices = hls::choices(&master, Some(100.0), false, None);
        let labels: Vec<_> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["1080p60", "720p", "360p"]);
        // 4 Mbit/s for 100 s.
        assert_eq!(choices[0].size, Some(50_000_000));
        assert_eq!(choices[1].format, MediaFormat::Video { max_height: Some(720) });
    }

    #[test]
    fn hls_separate_sound_needs_ffmpeg() {
        let text = "#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"English\",URI=\"audio.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,AUDIO=\"a\"
720.m3u8
";
        let master = hls::parse(text);
        assert!(master.separate_audio);
        let without: Vec<_> =
            hls::choices(&master, None, false, None).into_iter().map(|c| c.label).collect();
        assert_eq!(without, ["Audio only"]);
        assert_eq!(hls::choices(&master, None, true, None).len(), 2);
    }

    #[test]
    fn hls_media_playlist_is_one_quality() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:9.5,\na.ts\n#EXTINF:10,\nb.ts\n";
        assert!(is_hls(text));
        assert_eq!(hls::duration(text), Some(19.5));
        let choices = hls::choices(&hls::parse(text), Some(19.5), false, Some(1000));
        assert_eq!(choices[0].format, MediaFormat::Video { max_height: None });
        assert_eq!(choices[0].size, Some(1000));
        assert_eq!(hls::first_piece(text), Some((9.5, "a.ts")));
    }

    #[test]
    fn hls_drm_is_refused_and_aes_is_fine() {
        assert!(hls::check_keys("#EXT-X-KEY:METHOD=AES-128,URI=\"k\"").is_ok());
        assert!(hls::check_keys("#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"").is_err());
        let widevine = "#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,KEYFORMAT=\"urn:uuid:edef8ba9\"";
        assert!(hls::check_keys(widevine).is_err());
    }

    #[test]
    fn hls_pieces_carry_keys_ranges_and_the_init_piece() {
        let url = Url::parse("https://cdn.example/v/720/index.m3u8?t=1").unwrap();
        let text = "#EXTM3U
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"800@0\"
#EXT-X-KEY:METHOD=AES-128,URI=\"/keys/k1\"
#EXTINF:4.0,
#EXT-X-BYTERANGE:1000@800
all.mp4
#EXTINF:4.0,
#EXT-X-BYTERANGE:1200
all.mp4
#EXT-X-KEY:METHOD=AES-128,URI=\"k2\",IV=0x000102030405060708090A0B0C0D0E0F
#EXTINF:2.5,
b.m4s
#EXT-X-KEY:METHOD=NONE
#EXTINF:1,
c.m4s
#EXT-X-ENDLIST
";
        let track = hls::track(&url, text, TrackKind::Video, Some(1)).unwrap();
        assert_eq!(track.container, Container::Mp4);
        let p = &track.pieces;
        assert_eq!(p.len(), 5);
        assert_eq!(
            (p[0].url.as_str(), p[0].range, p[0].seconds),
            ("https://cdn.example/v/720/init.mp4", Some((0, 800)), 0.0)
        );
        assert_eq!(p[1].range, Some((800, 1000)));
        // Without an offset, a range follows the last one of the same file.
        assert_eq!(p[2].range, Some((1800, 1200)));
        let key = p[1].key.as_ref().unwrap();
        assert_eq!(key.url.as_str(), "https://cdn.example/keys/k1");
        // Without an IV, the media sequence number is one.
        assert_eq!(key.iv, 7u128.to_be_bytes());
        assert_eq!(p[2].key.as_ref().unwrap().iv, 8u128.to_be_bytes());
        assert_eq!(
            p[3].key.as_ref().unwrap().iv,
            0x0001_0203_0405_0607_0809_0A0B_0C0D_0E0Fu128.to_be_bytes()
        );
        assert!(p[4].key.is_none());
    }

    #[test]
    fn hls_live_streams_are_left_to_ytdlp() {
        let url = Url::parse("https://cdn.example/live.m3u8").unwrap();
        let live = "#EXTM3U\n#EXTINF:6,\na.ts\n#EXTINF:6,\nb.ts\n";
        assert!(matches!(hls::track(&url, live, TrackKind::Video, None), Err(Error::NotPlaylist)));
        let vod = format!("{live}#EXT-X-ENDLIST\n");
        let track = hls::track(&url, &vod, TrackKind::Video, None).unwrap();
        assert_eq!(track.container, Container::Ts);
        assert_eq!(track.pieces[1].url.as_str(), "https://cdn.example/b.ts");
    }

    #[test]
    fn hls_picks_the_height_and_its_sound() {
        let text = "#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"lo\",NAME=\"en\",URI=\"lo.m3u8\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"hi\",NAME=\"de\",URI=\"hi-de.m3u8\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"hi\",NAME=\"en\",DEFAULT=YES,URI=\"hi-en.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360,AUDIO=\"lo\"
360.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,AUDIO=\"hi\"
1080.m3u8
";
        let master = hls::parse(text);
        let pick = |h| hls::pick(&master, MediaFormat::Video { max_height: h }).unwrap();
        assert_eq!(pick(None).uri, "1080.m3u8");
        assert_eq!(pick(Some(720)).uri, "360.m3u8");
        // Taller than any asked for: the smallest there is.
        assert_eq!(pick(Some(240)).uri, "360.m3u8");
        let sound = |h| {
            let format = MediaFormat::Video { max_height: h };
            hls::sound_for(&master, hls::pick(&master, format), format).map(|r| r.uri.as_str())
        };
        assert_eq!(sound(None), Some("hi-en.m3u8"));
        assert_eq!(sound(Some(360)), Some("lo.m3u8"));
        // A variant with its sound inside needs no second track.
        assert!(
            hls::sound_for(
                &hls::parse(MASTER),
                hls::parse(MASTER).variants.first(),
                MediaFormat::Video { max_height: None }
            )
            .is_none()
        );
    }

    const MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" mediaPresentationDuration="PT1M40S" maxHeight="1080">
  <Period>
    <AdaptationSet mimeType="video/mp4" frameRate="30">
      <Representation id="1" bandwidth="1000000" width="1280" height="720"/>
      <Representation id="2" bandwidth="3000000" width="1920" height="1080"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <Representation id="a" bandwidth="128000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn dash_lists_heights_and_sound() {
        let manifest = dash::parse(MPD).unwrap();
        assert_eq!(manifest.duration, Some(100.0));
        let choices = dash::choices(&manifest, true);
        let labels: Vec<_> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["1080p", "720p", "Audio only"]);
        assert_eq!(choices[0].ext, "mp4");
        // (3 Mbit/s + 128 kbit/s) for 100 s.
        assert_eq!(choices[0].size, Some(39_100_000));
        // Without ffmpeg only the sound can be had.
        assert_eq!(dash::choices(&manifest, false).len(), 1);
    }

    #[test]
    fn dash_drm_is_refused() {
        let protected = MPD.replace("<Period>", "<Period><ContentProtection schemeIdUri=\"x\"/>");
        assert!(dash::parse(&protected).is_err());
    }

    #[test]
    fn reads_iso_durations() {
        assert_eq!(dash::iso_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(dash::iso_duration("PT45S"), Some(45.0));
        assert_eq!(dash::iso_duration("P0Y"), None);
    }

    /// Reads Mux's public test stream. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn reads_a_real_hls_stream() {
        let url = Url::parse("https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8").unwrap();
        let info = inspect(&Client::new(), &url, &[], true).await.unwrap();
        let labels: Vec<_> = info.choices.iter().map(|c| c.label.as_str()).collect();
        println!("{labels:?} {:?} {:?}", info.duration, info.choices[0].size);
        assert_eq!(info.site, "HLS");
        assert!(info.choices.len() > 1 && info.duration.is_some());
    }

    #[test]
    fn titles_come_from_the_file_name() {
        let url = Url::parse("https://cdn.example/v/My%20Movie.m3u8?token=1").unwrap();
        assert_eq!(title_from(&url), "My Movie");
        assert!(looks_like_playlist(&url));
        assert!(!looks_like_playlist(&Url::parse("https://example.com/watch?v=1").unwrap()));
    }
}
