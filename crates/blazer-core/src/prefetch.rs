//! Downloading from the moment a link is looked at, as IDM does: the response that told the add
//! dialog the file's size keeps arriving while the dialog is open, and becomes the download's
//! first connection when it starts. Without this, starting meant asking the server again and
//! waiting for every connection to get going from nothing.

use std::{
    collections::VecDeque,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use parking_lot::Mutex;
use tokio::{sync::oneshot, task::JoinHandle};

use crate::probe::ProbeInfo;

/// A response body, as the download reads it.
pub(crate) type Body = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// Most a prefetch keeps in memory. Then it stops reading, and the server waits.
const MAX_BUFFER: usize = 16 * 1024 * 1024;
/// How long a prefetch is kept for a download that has not started. Past this, a dialog has
/// likely been left open or closed, and the server has likely given up on the connection.
const KEEP: Duration = Duration::from_secs(5 * 60);
/// Most prefetches kept at once, so that looking at many links holds few connections.
const MAX_KEPT: usize = 3;

/// The start of a download that has not been added yet, or has not started yet.
pub(crate) struct Prefetch {
    url: String,
    headers: Vec<(String, String)>,
    pub info: ProbeInfo,
    at: Instant,
    buffer: Arc<Mutex<VecDeque<Bytes>>>,
    give_back: Option<oneshot::Sender<()>>,
    reader: Option<JoinHandle<Option<Body>>>,
}

impl Prefetch {
    /// Starts reading `resp`, the answer to a request for the whole file from its start.
    pub fn start(
        url: String,
        headers: Vec<(String, String)>,
        info: ProbeInfo,
        resp: reqwest::Response,
    ) -> Self {
        let buffer = Arc::new(Mutex::new(VecDeque::new()));
        let (give_back, mut asked) = oneshot::channel();
        let mut body: Body = Box::pin(resp.bytes_stream());
        let kept = buffer.clone();
        let reader = tokio::spawn(async move {
            let mut held = 0;
            let expiry = tokio::time::sleep(KEEP);
            tokio::pin!(expiry);
            loop {
                tokio::select! {
                    _ = &mut asked => return Some(body),
                    () = &mut expiry => return None,
                    next = body.next(), if held < MAX_BUFFER => match next {
                        Some(Ok(chunk)) => {
                            held += chunk.len();
                            kept.lock().push_back(chunk);
                        }
                        // What arrived is still good; the download asks for the rest.
                        Some(Err(e)) => {
                            tracing::debug!("prefetch connection ended: {e}");
                            return None;
                        }
                        None => return None,
                    },
                }
            }
        });
        Self {
            url,
            headers,
            info,
            at: Instant::now(),
            buffer,
            give_back: Some(give_back),
            reader: Some(reader),
        }
    }

    fn is_for(&self, url: &str, headers: &[(String, String)]) -> bool {
        self.url == url && self.headers == headers
    }

    fn is_fresh(&self) -> bool {
        self.at.elapsed() < KEEP
    }

    /// The file from its start: what arrived so far, then the rest as it arrives. Ends early if
    /// the connection was lost, which the download treats like any cut-off response.
    pub async fn into_body(mut self) -> Body {
        if let Some(give_back) = self.give_back.take() {
            let _ = give_back.send(());
        }
        let rest = match self.reader.take() {
            Some(reader) => reader.await.ok().flatten(),
            None => None,
        };
        let arrived: Vec<_> =
            std::mem::take(&mut *self.buffer.lock()).into_iter().map(Ok).collect();
        let arrived = stream::iter(arrived);
        match rest {
            Some(rest) => Box::pin(arrived.chain(rest)),
            None => Box::pin(arrived),
        }
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        // Closes the connection.
        if let Some(reader) = &self.reader {
            reader.abort();
        }
    }
}

/// Prefetches waiting to be used: by link while no download was added for them, and by
/// download once one was.
#[derive(Default)]
pub(crate) struct Prefetches {
    by_link: Vec<Prefetch>,
    by_download: Vec<(crate::types::DownloadId, Prefetch)>,
}

impl Prefetches {
    pub fn keep(&mut self, prefetch: Prefetch) {
        self.tidy();
        self.by_link.retain(|p| !p.is_for(&prefetch.url, &prefetch.headers));
        self.by_link.push(prefetch);
        while self.by_link.len() + self.by_download.len() > MAX_KEPT && !self.by_link.is_empty() {
            self.by_link.remove(0);
        }
    }

    /// The prefetch for this link, if one is fresh.
    pub fn take_link(&mut self, url: &str, headers: &[(String, String)]) -> Option<Prefetch> {
        self.tidy();
        let at = self.by_link.iter().position(|p| p.is_for(url, headers))?;
        Some(self.by_link.remove(at))
    }

    pub fn attach(&mut self, id: crate::types::DownloadId, prefetch: Prefetch) {
        self.tidy();
        self.by_download.push((id, prefetch));
        while self.by_link.len() + self.by_download.len() > MAX_KEPT {
            match self.by_link.is_empty() {
                true => drop(self.by_download.remove(0)),
                false => drop(self.by_link.remove(0)),
            }
        }
    }

    pub fn take_download(&mut self, id: crate::types::DownloadId) -> Option<Prefetch> {
        self.tidy();
        let at = self.by_download.iter().position(|(i, _)| *i == id)?;
        Some(self.by_download.remove(at).1)
    }

    fn tidy(&mut self) {
        self.by_link.retain(Prefetch::is_fresh);
        self.by_download.retain(|(_, p)| p.is_fresh());
    }
}
