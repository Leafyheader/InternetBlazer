//! DASH manifests: the qualities they offer, and the pieces of the ones picked.
//!
//! A manifest is XML, read by the small parser at the end, which keeps elements, attributes and
//! text. How a representation's pieces are named may be set on its period, its adaptation set or
//! itself, the lower overriding the higher, and the `BaseURL` at each level is relative to the
//! one above.

use std::cmp::Reverse;

use reqwest::Client;
use url::Url;

use super::{drm, estimate, label};
use crate::{
    error::{Error, Result},
    probe,
    stream::{Container, Piece, Track, TrackKind},
    types::{MediaChoice, MediaFormat},
};

/// A file that the manifest doesn't divide is fetched in parts of this size.
const PART: u64 = 8 * 1024 * 1024;
/// More pieces than this means a mistake in the manifest or in reading it.
const MAX_PIECES: u64 = 1_000_000;

#[derive(Debug, Default)]
pub(super) struct Manifest {
    pub duration: Option<f64>,
    /// Best first.
    pub videos: Vec<Representation>,
    pub audios: Vec<Representation>,
    /// A live stream, or one in several periods (as with ads between): yt-dlp downloads those.
    pub unsupported: bool,
}

#[derive(Debug)]
pub(super) struct Representation {
    pub bandwidth: Option<u64>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub mp4: bool,
    pub webm: bool,
    id: String,
    /// The `BaseURL`s from the manifest's down to its own, each relative to the one before.
    bases: Vec<String>,
    segments: Segments,
}

#[derive(Debug)]
enum Segments {
    /// Addresses made from a pattern, with each piece's number or start time.
    Template {
        media: String,
        init: Option<String>,
        start: u64,
        timescale: u64,
        /// Where the timeline's times start.
        offset: u64,
        /// The length of each piece, when there is no timeline.
        duration: Option<u64>,
        timeline: Vec<Repeat>,
    },
    /// Each piece listed.
    List { init: Option<Source>, pieces: Vec<Source>, timescale: u64, duration: Option<u64> },
    /// One file: the `BaseURL`.
    Whole,
}

/// A `<S t d r>` of a timeline: pieces of length `d` starting at `t`, `r` more times, or until
/// the next one or the end for -1.
#[derive(Debug)]
struct Repeat {
    t: Option<u64>,
    d: u64,
    r: i64,
}

/// A piece's address relative to the base, and its byte range; without an address it is part of
/// the base file.
#[derive(Debug)]
struct Source {
    url: Option<String>,
    range: Option<(u64, u64)>,
}

pub(super) fn parse(text: &str) -> Result<Manifest> {
    if text.contains("<ContentProtection") || text.contains(":ContentProtection") {
        return Err(drm());
    }
    let root = xml::parse(text).filter(|e| e.name == "MPD").ok_or(Error::NotPlaylist)?;
    let periods: Vec<&xml::Element> = root.children("Period").collect();
    let mut manifest = Manifest {
        duration: root.attr("mediaPresentationDuration").and_then(iso_duration),
        unsupported: root.attr("type") == Some("dynamic") || periods.len() != 1,
        ..Default::default()
    };
    let Some(&period) = periods.first() else { return Ok(manifest) };
    if manifest.duration.is_none() {
        manifest.duration = period.attr("duration").and_then(iso_duration);
    }
    for set in period.children("AdaptationSet") {
        let set_kind = kind(set);
        for rep in set.children("Representation") {
            let attr = |name| rep.attr(name).or_else(|| set.attr(name));
            let mime = attr("mimeType");
            let found = Representation {
                bandwidth: rep.attr("bandwidth").and_then(|b| b.parse().ok()),
                height: attr("height").and_then(|h| h.parse().ok()),
                fps: attr("frameRate").and_then(frame_rate),
                mp4: mime.is_none_or(|m| m.contains("mp4")),
                webm: mime.is_some_and(|m| m.contains("webm")),
                id: rep.attr("id").unwrap_or_default().to_string(),
                bases: [&root, period, set, rep]
                    .iter()
                    .filter_map(|e| e.child("BaseURL"))
                    .map(|b| b.text.trim().to_string())
                    .collect(),
                segments: segments(&[period, set, rep]),
            };
            match kind(rep).or(set_kind) {
                Some(TrackKind::Video) => manifest.videos.push(found),
                Some(TrackKind::Audio) => manifest.audios.push(found),
                None => {}
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
            let bandwidth = v.bandwidth.map(|b| b + sound.and_then(|s| s.bandwidth).unwrap_or(0));
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

/// The representations for `format`, as the choices offered them: the best picture up to the
/// height asked for (or the smallest when all are taller) with the best sound, or the sound alone.
pub(super) fn pick(manifest: &Manifest, format: MediaFormat) -> Vec<(&Representation, TrackKind)> {
    let video = match format {
        MediaFormat::Audio => None,
        MediaFormat::Video { max_height: Some(max) } => manifest
            .videos
            .iter()
            .find(|v| v.height.is_none_or(|h| h <= max))
            .or(manifest.videos.last()),
        MediaFormat::Video { max_height: None } => manifest.videos.first(),
    };
    let video = video.map(|v| (v, TrackKind::Video));
    let sound = manifest.audios.first().map(|a| (a, TrackKind::Audio));
    video.into_iter().chain(sound).collect()
}

/// The pieces of `rep`, a representation of the manifest at `url`.
pub(super) async fn track(
    client: &Client,
    headers: &[(String, String)],
    url: &Url,
    manifest: &Manifest,
    rep: &Representation,
    kind: TrackKind,
) -> Result<Track> {
    let invalid = |e: url::ParseError| Error::Invalid(e.to_string());
    let base = rep.bases.iter().try_fold(url.clone(), |url, b| url.join(b)).map_err(invalid)?;
    let piece = |url: Url, range, seconds| Piece { url, range, key: None, seconds };
    let mut pieces = Vec::new();
    match &rep.segments {
        Segments::Template { media, init, start, timescale, offset, duration, timeline } => {
            let fill =
                |pattern: &str, number, time| fill(pattern, &rep.id, rep.bandwidth, number, time);
            if let Some(init) = init {
                pieces.push(piece(base.join(&fill(init, 0, 0)).map_err(invalid)?, None, 0.0));
            }
            let scale = *timescale as f64;
            let mut number = *start;
            let mut add = |time: u64, length: u64| -> Result<()> {
                let url = base.join(&fill(media, number, time)).map_err(invalid)?;
                pieces.push(piece(url, None, length as f64 / scale));
                number += 1;
                Ok(())
            };
            if timeline.is_empty() {
                let length = duration.filter(|&d| d > 0).ok_or(Error::NotPlaylist)?;
                let seconds = manifest.duration.ok_or(Error::NotPlaylist)?;
                let count = (seconds * scale / length as f64).ceil() as u64;
                if count > MAX_PIECES {
                    return Err(Error::NotPlaylist);
                }
                for i in 0..count {
                    add(offset + i * length, length)?;
                }
            } else {
                let end = manifest.duration.map(|d| offset + (d * scale) as u64);
                let mut time = *offset;
                let mut count = 0;
                for (i, s) in timeline.iter().enumerate() {
                    if s.d == 0 {
                        return Err(Error::NotPlaylist);
                    }
                    time = s.t.unwrap_or(time);
                    let repeats = match u64::try_from(s.r) {
                        Ok(r) => r + 1,
                        Err(_) => {
                            let until = timeline.get(i + 1).and_then(|n| n.t).or(end);
                            until.ok_or(Error::NotPlaylist)?.saturating_sub(time).div_ceil(s.d)
                        }
                    };
                    count += repeats;
                    if count > MAX_PIECES {
                        return Err(Error::NotPlaylist);
                    }
                    for _ in 0..repeats {
                        add(time, s.d)?;
                        time += s.d;
                    }
                }
            }
        }
        Segments::List { init, pieces: sources, timescale, duration } => {
            let source = |s: &Source| match &s.url {
                Some(u) => base.join(u).map_err(invalid),
                None => Ok(base.clone()),
            };
            if let Some(init) = init {
                pieces.push(piece(source(init)?, init.range, 0.0));
            }
            let seconds = match (duration, manifest.duration) {
                (Some(d), _) => *d as f64 / *timescale as f64,
                (None, Some(total)) => total / sources.len().max(1) as f64,
                (None, None) => 0.0,
            };
            for s in sources {
                pieces.push(piece(source(s)?, s.range, seconds));
            }
        }
        Segments::Whole => {
            let info = probe::probe(client, base.as_str(), headers).await?;
            match info.total.filter(|&t| t > 0 && info.resumable) {
                Some(total) => {
                    for i in 0..total.div_ceil(PART) {
                        let start = i * PART;
                        let len = PART.min(total - start);
                        let seconds = manifest.duration.unwrap_or(0.0) * len as f64 / total as f64;
                        pieces.push(piece(base.clone(), Some((start, len)), seconds));
                    }
                }
                None => pieces.push(piece(base.clone(), None, manifest.duration.unwrap_or(0.0))),
            }
        }
    }
    if pieces.is_empty() {
        return Err(Error::NotPlaylist);
    }
    let container = if rep.webm { Container::WebM } else { Container::Mp4 };
    Ok(Track { kind, container, pieces, bandwidth: rep.bandwidth })
}

/// How the pieces of a representation are named, from the lowest of `levels` that says.
fn segments(levels: &[&xml::Element]) -> Segments {
    let number = |text: &str| text.trim().parse::<u64>().ok();
    let templates: Vec<&xml::Element> =
        levels.iter().filter_map(|l| l.child("SegmentTemplate")).collect();
    let get = |name: &str| templates.iter().rev().find_map(|t| t.attr(name));
    if let Some(media) = get("media") {
        let timeline = templates
            .iter()
            .rev()
            .find_map(|t| t.child("SegmentTimeline"))
            .map(|timeline| {
                timeline
                    .children("S")
                    .map(|s| Repeat {
                        t: s.attr("t").and_then(number),
                        d: s.attr("d").and_then(number).unwrap_or(0),
                        r: s.attr("r").and_then(|r| r.trim().parse().ok()).unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Segments::Template {
            media: media.to_string(),
            init: get("initialization").map(str::to_string),
            start: get("startNumber").and_then(number).unwrap_or(1),
            timescale: get("timescale").and_then(number).unwrap_or(1).max(1),
            offset: get("presentationTimeOffset").and_then(number).unwrap_or(0),
            duration: get("duration").and_then(number),
            timeline,
        };
    }
    if let Some(list) = levels.iter().rev().find_map(|l| l.child("SegmentList")) {
        let source = |e: &xml::Element, url: &str, range: &str| Source {
            url: e.attr(url).map(str::to_string),
            range: e.attr(range).and_then(byte_range),
        };
        return Segments::List {
            init: list.child("Initialization").map(|i| source(i, "sourceURL", "range")),
            pieces: list.children("SegmentURL").map(|s| source(s, "media", "mediaRange")).collect(),
            timescale: list.attr("timescale").and_then(number).unwrap_or(1).max(1),
            duration: list.attr("duration").and_then(number),
        };
    }
    Segments::Whole
}

/// Fills in a `SegmentTemplate` pattern: `$RepresentationID$`, `$Number$`, `$Time$` and
/// `$Bandwidth$`, the numbers with a width such as `$Number%05d$` if given, and `$$` for a dollar.
fn fill(pattern: &str, id: &str, bandwidth: Option<u64>, number: u64, time: u64) -> String {
    let mut out = String::new();
    // Text and identifiers take turns between the dollars.
    for (i, part) in pattern.split('$').enumerate() {
        if i % 2 == 0 {
            out.push_str(part);
            continue;
        }
        if part.is_empty() {
            out.push('$');
            continue;
        }
        let (name, width) = match part.split_once('%') {
            Some((name, format)) => (name, format.trim_end_matches('d').parse::<usize>().ok()),
            None => (part, None),
        };
        let value = match name {
            "RepresentationID" => {
                out.push_str(id);
                continue;
            }
            "Number" => number,
            "Time" => time,
            "Bandwidth" => bandwidth.unwrap_or(0),
            _ => {
                out.push('$');
                out.push_str(part);
                out.push('$');
                continue;
            }
        };
        out.push_str(&format!("{value:0width$}", width = width.unwrap_or(0)));
    }
    out
}

/// `start-end`, inclusive, as start and length.
fn byte_range(text: &str) -> Option<(u64, u64)> {
    let (start, end) = text.trim().split_once('-')?;
    let (start, end): (u64, u64) = (start.parse().ok()?, end.parse().ok()?);
    (end >= start).then(|| (start, end - start + 1))
}

fn kind(e: &xml::Element) -> Option<TrackKind> {
    let text = e.attr("contentType").or_else(|| e.attr("mimeType"))?;
    if text.starts_with("video") {
        Some(TrackKind::Video)
    } else if text.starts_with("audio") {
        Some(TrackKind::Audio)
    } else {
        None
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

/// Enough XML for manifests: elements without their namespace prefix, attributes, and text,
/// with the predefined and numeric entities. No DTDs.
mod xml {
    #[derive(Debug, Default)]
    pub(super) struct Element {
        pub name: String,
        attrs: Vec<(String, String)>,
        children: Vec<Element>,
        pub text: String,
    }

    impl Element {
        pub(super) fn attr(&self, name: &str) -> Option<&str> {
            self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        }

        pub(super) fn child(&self, name: &str) -> Option<&Element> {
            self.children.iter().find(|c| c.name == name)
        }

        pub(super) fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
            self.children.iter().filter(move |c| c.name == name)
        }
    }

    /// The root element, or `None` for text that isn't XML.
    pub(super) fn parse(text: &str) -> Option<Element> {
        // The bottom one holds the root.
        let mut stack = vec![Element::default()];
        let mut rest = text;
        while let Some(lt) = rest.find('<') {
            let text = decode(&rest[..lt]);
            stack.last_mut()?.text.push_str(&text);
            rest = &rest[lt..];
            if let Some(after) = rest.strip_prefix("<!--") {
                rest = &after[after.find("-->")? + 3..];
            } else if let Some(after) = rest.strip_prefix("<![CDATA[") {
                let end = after.find("]]>")?;
                stack.last_mut()?.text.push_str(&after[..end]);
                rest = &after[end + 3..];
            } else if rest.starts_with("<?") || rest.starts_with("<!") {
                rest = &rest[rest.find('>')? + 1..];
            } else if let Some(after) = rest.strip_prefix("</") {
                rest = &after[after.find('>')? + 1..];
                if stack.len() == 1 {
                    return None;
                }
                let done = stack.pop()?;
                stack.last_mut()?.children.push(done);
            } else {
                let end = tag_end(rest)?;
                let tag = &rest[1..end];
                rest = &rest[end + 1..];
                match tag.strip_suffix('/') {
                    Some(tag) => stack.last_mut()?.children.push(element(tag)),
                    None => stack.push(element(tag)),
                }
            }
        }
        // Elements still open, as at the end of a manifest cut short.
        while stack.len() > 1 {
            let done = stack.pop()?;
            stack.last_mut()?.children.push(done);
        }
        stack.pop()?.children.into_iter().next()
    }

    /// Where the tag that `text` starts with ends, at a `>` outside quotes.
    fn tag_end(text: &str) -> Option<usize> {
        let mut quote = None;
        for (i, c) in text.char_indices() {
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == '"' || c == '\'' => quote = Some(c),
                None if c == '>' => return Some(i),
                None => {}
            }
        }
        None
    }

    fn element(tag: &str) -> Element {
        let tag = tag.trim();
        let name_end = tag.find(char::is_whitespace).unwrap_or(tag.len());
        let name = &tag[..name_end];
        let mut attrs = Vec::new();
        let mut rest = &tag[name_end..];
        while let Some(eq) = rest.find('=') {
            let key = rest[..eq].trim();
            let value = rest[eq + 1..].trim_start();
            let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
                break;
            };
            let Some(close) = value[1..].find(quote) else { break };
            attrs.push((key.to_string(), decode(&value[1..=close])));
            rest = &value[close + 2..];
        }
        Element {
            name: name.rsplit(':').next().unwrap_or(name).to_string(),
            attrs,
            ..Default::default()
        }
    }

    fn decode(text: &str) -> String {
        if !text.contains('&') {
            return text.to_string();
        }
        let mut out = String::new();
        let mut rest = text;
        while let Some(amp) = rest.find('&') {
            out.push_str(&rest[..amp]);
            rest = &rest[amp..];
            let entity = rest.find(';').map(|semi| (&rest[1..semi], semi));
            let found = entity.and_then(|(name, semi)| {
                let c = match name {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    _ => {
                        let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X"))
                        {
                            Some(hex) => u32::from_str_radix(hex, 16).ok(),
                            None => name.strip_prefix('#').and_then(|d| d.parse().ok()),
                        };
                        char::from_u32(code?)?
                    }
                };
                Some((c, semi))
            });
            match found {
                Some((c, semi)) => {
                    out.push(c);
                    rest = &rest[semi + 1..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reads_nesting_attributes_and_text() {
            let root = parse(
                "<?xml version=\"1.0\"?><!-- note --><mpd:MPD a='x &amp; y' b=\"1>2\">\
                 <BaseURL> https://cdn/ </BaseURL><Period><S t=\"1\"/><S/></Period></mpd:MPD>",
            )
            .unwrap();
            assert_eq!(root.name, "MPD");
            assert_eq!(root.attr("a"), Some("x & y"));
            assert_eq!(root.attr("b"), Some("1>2"));
            assert_eq!(root.child("BaseURL").unwrap().text.trim(), "https://cdn/");
            assert_eq!(root.child("Period").unwrap().children("S").count(), 2);
            assert!(parse("not xml").is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATED: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT9.5S">
  <BaseURL>https://cdn.example/v/</BaseURL>
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <BaseURL>video/</BaseURL>
      <SegmentTemplate timescale="1000" duration="4000" startNumber="3"
          initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/$Number%04d$.m4s"/>
      <Representation id="v720" bandwidth="2000000" height="720"/>
      <Representation id="v1080" bandwidth="4000000" height="1080"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" lang="en">
      <SegmentTemplate timescale="48000" initialization="a/init-$Bandwidth$.mp4" media="a/t$Time$.m4s">
        <SegmentTimeline><S t="100" d="96000" r="1"/><S d="48000" r="-1"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="a1" bandwidth="128000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    fn urls(track: &Track) -> Vec<&str> {
        track.pieces.iter().map(|p| p.url.as_str()).collect()
    }

    async fn tracks_of(text: &str, format: MediaFormat) -> Vec<Track> {
        let manifest = parse(text).unwrap();
        let url = Url::parse("https://site.example/manifest.mpd").unwrap();
        let mut tracks = Vec::new();
        for (rep, kind) in pick(&manifest, format) {
            tracks.push(track(&Client::new(), &[], &url, &manifest, rep, kind).await.unwrap());
        }
        tracks
    }

    #[tokio::test]
    async fn templates_number_and_time_the_pieces() {
        let tracks = tracks_of(TEMPLATED, MediaFormat::Video { max_height: Some(720) }).await;
        assert_eq!(tracks.len(), 2);
        // 9.5 s in pieces of 4 s.
        assert_eq!(
            urls(&tracks[0]),
            [
                "https://cdn.example/v/video/v720/init.mp4",
                "https://cdn.example/v/video/v720/0003.m4s",
                "https://cdn.example/v/video/v720/0004.m4s",
                "https://cdn.example/v/video/v720/0005.m4s",
            ]
        );
        assert_eq!(tracks[0].pieces[1].seconds, 4.0);
        // Two pieces of 2 s from 100, then 1 s pieces up to the end at 9.5 s.
        let sound = urls(&tracks[1]);
        assert_eq!(sound[0], "https://cdn.example/v/a/init-128000.mp4");
        assert_eq!(
            &sound[1..4],
            [
                "https://cdn.example/v/a/t100.m4s",
                "https://cdn.example/v/a/t96100.m4s",
                "https://cdn.example/v/a/t192100.m4s"
            ]
        );
        assert_eq!(sound.len(), 1 + 2 + 6);
        assert_eq!(tracks[1].kind, TrackKind::Audio);
        // The sound alone.
        let tracks = tracks_of(TEMPLATED, MediaFormat::Audio).await;
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].kind, TrackKind::Audio);
    }

    #[tokio::test]
    async fn lists_name_each_piece() {
        let text = r#"<MPD mediaPresentationDuration="PT8S"><Period><AdaptationSet contentType="video">
          <Representation id="1" height="480" mimeType="video/webm"><BaseURL>v.webm</BaseURL>
            <SegmentList timescale="10" duration="40">
              <Initialization range="0-99"/>
              <SegmentURL mediaRange="100-599"/><SegmentURL media="b.webm"/>
            </SegmentList>
          </Representation></AdaptationSet></Period></MPD>"#;
        let tracks = tracks_of(text, MediaFormat::Video { max_height: None }).await;
        let pieces = &tracks[0].pieces;
        assert_eq!(tracks[0].container, Container::WebM);
        assert_eq!(
            (pieces[0].url.as_str(), pieces[0].range),
            ("https://site.example/v.webm", Some((0, 100)))
        );
        assert_eq!((pieces[1].range, pieces[1].seconds), (Some((100, 500)), 4.0));
        assert_eq!(pieces[2].url.as_str(), "https://site.example/b.webm");
    }

    #[test]
    fn live_and_several_periods_are_left_to_ytdlp() {
        let live = TEMPLATED.replace("type=\"static\"", "type=\"dynamic\"");
        assert!(parse(&live).unwrap().unsupported);
        let two = TEMPLATED.replace("</Period>", "</Period><Period></Period>");
        assert!(parse(&two).unwrap().unsupported);
        assert!(!parse(TEMPLATED).unwrap().unsupported);
    }

    #[test]
    fn fills_in_templates() {
        assert_eq!(
            fill("$RepresentationID$-$Number%05d$-$Time$-$$.m4s", "v", None, 42, 7),
            "v-00042-7-$.m4s"
        );
        assert_eq!(fill("$Bandwidth$/$Other$", "v", Some(5), 0, 0), "5/$Other$");
    }
}
