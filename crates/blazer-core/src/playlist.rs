//! Reads the HLS and DASH playlists that video players fetch, to list a stream's qualities in a
//! moment, where asking yt-dlp takes seconds. yt-dlp still downloads the stream, given the
//! playlist and the height picked, which it matches to the same qualities.
//!
//! An HLS master playlist lists one variant per quality, each with its own playlist of pieces;
//! a DASH manifest lists the picture and the sound separately, which ffmpeg joins.

use std::cmp::Reverse;

use reqwest::Client;
use url::Url;

use crate::{
    error::{Error, Result},
    probe,
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
    }

    #[derive(Debug)]
    pub(super) struct Variant {
        pub uri: String,
        pub bandwidth: Option<u64>,
        pub height: Option<u32>,
        pub fps: Option<f64>,
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
                if is_audio && get(&attrs, "URI").is_some() {
                    master.separate_audio = true;
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
                master.variants.push(Variant { uri: line.to_string(), bandwidth, height, fps });
            }
        }
        master.variants.sort_by_key(|v| Reverse((v.height, v.bandwidth)));
        master
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

mod dash {
    use super::*;

    #[derive(Debug, Default)]
    pub(super) struct Manifest {
        pub duration: Option<f64>,
        pub videos: Vec<Representation>,
        pub audios: Vec<Representation>,
    }

    #[derive(Debug)]
    pub(super) struct Representation {
        pub bandwidth: Option<u64>,
        pub height: Option<u32>,
        pub fps: Option<f64>,
        pub mp4: bool,
    }

    /// Reads what the qualities need from the manifest's XML, without a full XML parser.
    pub(super) fn parse(text: &str) -> Result<Manifest> {
        if text.contains("<ContentProtection") || text.contains(":ContentProtection") {
            return Err(drm());
        }
        let mut manifest = Manifest {
            duration: tag(text, "MPD")
                .and_then(|t| attr(t, "mediaPresentationDuration"))
                .and_then(iso_duration),
            ..Default::default()
        };
        for set in text.split("<AdaptationSet").skip(1) {
            let set = set.split("</AdaptationSet>").next().unwrap_or(set);
            let set_tag = set.split('>').next().unwrap_or_default();
            let set_kind = kind(set_tag);
            for rep in set.split("<Representation").skip(1) {
                let rep_tag = rep.split('>').next().unwrap_or_default();
                let mime = attr(rep_tag, "mimeType").or_else(|| attr(set_tag, "mimeType"));
                let (video, audio) = match kind(rep_tag).or(set_kind) {
                    Some(k) => (k == "video", k == "audio"),
                    None => (false, false),
                };
                let found = Representation {
                    bandwidth: attr(rep_tag, "bandwidth").and_then(|b| b.parse().ok()),
                    height: attr(rep_tag, "height")
                        .or_else(|| attr(set_tag, "height"))
                        .and_then(|h| h.parse().ok()),
                    fps: attr(rep_tag, "frameRate")
                        .or_else(|| attr(set_tag, "frameRate"))
                        .and_then(frame_rate),
                    mp4: mime.is_none_or(|m| m.contains("mp4")),
                };
                if video {
                    manifest.videos.push(found);
                } else if audio {
                    manifest.audios.push(found);
                }
            }
        }
        manifest.videos.sort_by_key(|v| Reverse((v.height, v.mp4, v.bandwidth)));
        manifest.audios.sort_by_key(|a| Reverse((a.mp4, a.bandwidth)));
        Ok(manifest)
    }

    pub(super) fn choices(manifest: &Manifest, ffmpeg: bool) -> Vec<MediaChoice> {
        let sound = manifest.audios.first();
        let mut choices = Vec::new();
        // DASH always keeps picture and sound apart, so a video needs ffmpeg.
        if ffmpeg {
            let mut heights = Vec::new();
            for v in &manifest.videos {
                let Some(height) = v.height else { continue };
                if heights.contains(&height) {
                    continue;
                }
                heights.push(height);
                let bandwidth =
                    v.bandwidth.map(|b| b + sound.and_then(|s| s.bandwidth).unwrap_or(0));
                choices.push(MediaChoice {
                    label: label(height, v.fps),
                    format: MediaFormat::Video { max_height: Some(height) },
                    ext: if v.mp4 && sound.is_none_or(|s| s.mp4) { "mp4" } else { "mkv" }.into(),
                    size: estimate(bandwidth, manifest.duration),
                });
            }
        }
        if let Some(sound) = sound {
            choices.push(MediaChoice {
                label: "Audio only".into(),
                format: MediaFormat::Audio,
                ext: if sound.mp4 { "m4a" } else { "webm" }.into(),
                size: estimate(sound.bandwidth, manifest.duration),
            });
        }
        choices
    }

    fn kind(tag: &str) -> Option<&'static str> {
        let text = attr(tag, "contentType").or_else(|| attr(tag, "mimeType"))?;
        if text.starts_with("video") {
            Some("video")
        } else if text.starts_with("audio") {
            Some("audio")
        } else {
            None
        }
    }

    /// The text of the first `<name ...>` tag.
    fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
        let start = text.find(&format!("<{name}"))?;
        text[start..].split('>').next()
    }

    /// The value of ` name="value"` in a tag.
    fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        let mut search = tag;
        loop {
            let at = search.find(name)?;
            let before = search[..at].chars().next_back();
            let after = &search[at + name.len()..];
            if before.is_some_and(char::is_whitespace)
                && let Some(value) = after.strip_prefix("=\"").or_else(|| after.strip_prefix("='"))
            {
                return value.split(['"', '\'']).next();
            }
            search = after;
        }
    }

    /// `30000/1001` or `25`.
    fn frame_rate(text: &str) -> Option<f64> {
        match text.split_once('/') {
            Some((a, b)) => Some(a.parse::<f64>().ok()? / b.parse::<f64>().ok()?),
            None => text.parse().ok(),
        }
    }

    /// `PT1H2M3.5S`, as DASH writes lengths.
    pub(super) fn iso_duration(text: &str) -> Option<f64> {
        let time = text.strip_prefix("PT").or_else(|| text.split_once('T').map(|(_, t)| t))?;
        let mut total = 0.0;
        let mut number = String::new();
        for c in time.chars() {
            match c {
                'H' | 'M' | 'S' => {
                    let value: f64 = number.parse().ok()?;
                    total += value
                        * match c {
                            'H' => 3600.0,
                            'M' => 60.0,
                            _ => 1.0,
                        };
                    number.clear();
                }
                _ => number.push(c),
            }
        }
        (total > 0.0).then_some(total)
    }
}

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
