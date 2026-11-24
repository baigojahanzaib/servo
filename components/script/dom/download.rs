/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Feather: download support. A [`DownloadSpool`] streams a response body to
//! a temporary file which is handed to the embedder at EOF, so the embedder
//! never has to fetch the resource a second time. It is shared by the
//! document-navigation path (`servoparser`) and `<a download>` fetches
//! ([`AnchorDownloadFetchContext`]).

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use content_security_policy::Violation;
use js::context::JSContext;
use net_traits::request::RequestId;
use net_traits::{FetchMetadata, NetworkError, ResourceFetchTiming};
use servo_base::id::{PipelineId, WebViewId};
use servo_constellation_traits::ScriptToConstellationMessage;
use servo_url::ServoUrl;

use crate::dom::globalscope::GlobalScope;
use crate::fetch::network_listener::FetchResponseListener;

/// Bytes buffered between progress reports to the embedder.
const PROGRESS_GRANULARITY: u64 = 1024 * 1024;

/// A download body being streamed into a temporary file. Deletes the file on
/// drop unless [`DownloadSpool::finish`] consumed it.
pub(crate) struct DownloadSpool {
    file: File,
    path: Option<PathBuf>,
    written: u64,
    last_progress: u64,
}

impl DownloadSpool {
    pub(crate) fn new() -> Option<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "servo-download-{}-{nanos:x}.part",
            std::process::id()
        ));
        let file = File::create(&path).ok()?;
        Some(Self {
            file,
            path: Some(path),
            written: 0,
            last_progress: 0,
        })
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// True when at least [`PROGRESS_GRANULARITY`] new bytes have been written
    /// since the last report.
    pub(crate) fn progress_due(&mut self) -> bool {
        if self.written - self.last_progress >= PROGRESS_GRANULARITY {
            self.last_progress = self.written;
            return true;
        }
        false
    }

    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// Flush and hand the temp file path to the caller; the file is no longer
    /// deleted on drop.
    pub(crate) fn finish(mut self) -> PathBuf {
        let _ = self.file.flush();
        self.path.take().expect("spool finished twice")
    }
}

impl Drop for DownloadSpool {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Sends a `DownloadFinished` message for `webview_id`/`pipeline_id`, either
/// with the spooled `body_path` or, on failure, an `error` (the spool is
/// deleted by `DownloadSpool`'s `Drop`).
pub(crate) fn send_download_finished(
    global: &GlobalScope,
    webview_id: WebViewId,
    pipeline_id: PipelineId,
    url: ServoUrl,
    spool: Option<DownloadSpool>,
    status: &Result<(), NetworkError>,
    forced_error: Option<String>,
) {
    let (body_path, error) = match (spool, status, forced_error) {
        (_, Err(e), _) => (None, Some(format!("{e:?}"))),
        (_, _, Some(e)) => (None, Some(e)),
        (Some(spool), Ok(()), None) => (Some(spool.finish()), None),
        (None, Ok(()), None) => (None, Some("could not spool download body".to_owned())),
    };
    let _ = global
        .script_to_constellation_chan()
        .send(ScriptToConstellationMessage::DownloadFinished {
            webview_id,
            pipeline_id,
            url,
            body_path,
            error,
        });
}

/// Fetch listener for `<a download>` activations: notifies the embedder once
/// the response metadata is known, streams the body into a [`DownloadSpool`],
/// and reports progress/completion through the same messages as navigation
/// downloads.
pub(crate) struct AnchorDownloadFetchContext {
    pub(crate) webview_id: WebViewId,
    pub(crate) pipeline_id: PipelineId,
    /// The URL the anchor pointed at; keys the embedder's pending download.
    pub(crate) url: ServoUrl,
    /// Value of the `download` attribute (a filename hint); empty = none.
    pub(crate) filename_hint: Option<String>,
    spool: Option<DownloadSpool>,
    spool_failed: bool,
    total: Option<u64>,
}

impl AnchorDownloadFetchContext {
    pub(crate) fn new(
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        url: ServoUrl,
        filename_hint: Option<String>,
    ) -> Self {
        Self {
            webview_id,
            pipeline_id,
            url,
            filename_hint,
            spool: None,
            spool_failed: false,
            total: None,
        }
    }

    fn send_to_constellation(&self, global: &GlobalScope, msg: ScriptToConstellationMessage) {
        let _ = global.script_to_constellation_chan().send(msg);
    }
}

impl FetchResponseListener for AnchorDownloadFetchContext {
    fn process_request_body(&mut self, _: &GlobalScope, _: RequestId) {}

    fn process_response(
        &mut self,
        _: &mut JSContext,
        global: &GlobalScope,
        _: RequestId,
        metadata: Result<FetchMetadata, NetworkError>,
    ) {
        match metadata {
            Ok(fetch_metadata) => {
                let metadata = fetch_metadata.metadata();
                let headers = metadata.headers.as_ref();
                let content_disposition = headers
                    .and_then(|headers| headers.get("Content-Disposition"))
                    .and_then(|header| header.to_str().ok())
                    .map(|value| value.to_owned());
                let content_length = headers
                    .and_then(|headers| headers.get("Content-Length"))
                    .and_then(|header| header.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok());
                let mime = metadata
                    .content_type
                    .as_ref()
                    .map(|content_type| content_type.clone().into_inner().to_string())
                    .unwrap_or_default();
                self.total = content_length;
                self.spool = DownloadSpool::new();
                self.spool_failed = self.spool.is_none();
                self.send_to_constellation(
                    global,
                    ScriptToConstellationMessage::DownloadRequested {
                        webview_id: self.webview_id,
                        pipeline_id: self.pipeline_id,
                        url: self.url.clone(),
                        mime,
                        content_disposition,
                        filename_hint: self.filename_hint.clone(),
                        content_length,
                    },
                );
            },
            Err(error) => {
                send_download_finished(
                    global,
                    self.webview_id,
                    self.pipeline_id,
                    self.url.clone(),
                    None,
                    &Ok(()),
                    Some(format!("{error:?}")),
                );
            },
        }
    }

    fn process_response_chunk(
        &mut self,
        _: &mut JSContext,
        global: &GlobalScope,
        _: RequestId,
        chunk: Bytes,
    ) {
        let Some(spool) = &mut self.spool else {
            return;
        };
        if spool.append(&chunk).is_err() {
            self.spool_failed = true;
            self.spool = None;
            return;
        }
        let progress_due = spool.progress_due();
        let received = spool.written();
        if progress_due {
            self.send_to_constellation(
                global,
                ScriptToConstellationMessage::DownloadProgress {
                    webview_id: self.webview_id,
                    pipeline_id: self.pipeline_id,
                    url: self.url.clone(),
                    received,
                    total: self.total,
                },
            );
        }
    }

    fn process_response_eof(
        mut self,
        _: &mut JSContext,
        global: &GlobalScope,
        _: RequestId,
        response: Result<(), NetworkError>,
        _: ResourceFetchTiming,
    ) {
        let forced_error = self
            .spool_failed
            .then(|| "could not spool download body".to_owned());
        send_download_finished(
            global,
            self.webview_id,
            self.pipeline_id,
            self.url.clone(),
            self.spool.take(),
            &response,
            forced_error,
        );
    }

    fn process_csp_violations(
        &mut self,
        _: &mut JSContext,
        _: &GlobalScope,
        _: RequestId,
        _: Vec<Violation>,
    ) {
    }
}
