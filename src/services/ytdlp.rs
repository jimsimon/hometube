//! yt-dlp subprocess wrapper.
//!
//! HomeTube serves every video through a server-side proxy fed by
//! yt-dlp's `--dump-json` output. This module spawns yt-dlp with a hard
//! 30-second timeout, parses the resulting JSON into a strongly-typed
//! [`ExtractResult`], and exposes a thin [`version`] helper used by the
//! Phase 12 update job.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::config::Config;
use crate::error::{AppError, AppResult};

/// Default timeout for any single yt-dlp invocation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One format/quality entry from yt-dlp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Format {
    /// yt-dlp's internal format identifier (e.g., `"137"`, `"251"`).
    pub format_id: String,
    #[serde(default)]
    pub ext: Option<String>,
    #[serde(default)]
    pub height: Option<i64>,
    #[serde(default)]
    pub width: Option<i64>,
    /// Total bitrate (kbit/s).
    #[serde(default)]
    pub tbr: Option<f64>,
    /// Video bitrate.
    #[serde(default)]
    pub vbr: Option<f64>,
    /// Audio bitrate.
    #[serde(default)]
    pub abr: Option<f64>,
    #[serde(default)]
    pub fps: Option<f64>,
    #[serde(default)]
    pub vcodec: Option<String>,
    #[serde(default)]
    pub acodec: Option<String>,
    #[serde(default)]
    pub filesize: Option<i64>,
    #[serde(default)]
    pub url: Option<String>,
    /// `"https"`, `"http_dash_segments"`, etc.
    #[serde(default)]
    pub protocol: Option<String>,
    /// BCP-47 language tag for audio formats (e.g. `"en"`, `"es-MX"`).
    /// Absent on video-only formats.
    #[serde(default)]
    pub language: Option<String>,
    /// yt-dlp's heuristic preference for this language. The "original"
    /// audio for a video typically scores `10`, with auto-dubs ranking
    /// lower. Used by [`crate::services::dash::synthesize_manifest`] to
    /// pick the AdaptationSet to mark as `Role=main`.
    #[serde(default)]
    pub language_preference: Option<i64>,
    /// Free-form note from yt-dlp (e.g. `"original (default), low"`).
    /// Contains the substring `"original"` for the original-language
    /// audio track on multi-audio videos — used as a fallback signal
    /// when `language_preference` is absent.
    #[serde(default)]
    pub format_note: Option<String>,
}

/// One thumbnail variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thumbnail {
    pub url: String,
    #[serde(default)]
    pub width: Option<i64>,
    #[serde(default)]
    pub height: Option<i64>,
    #[serde(default)]
    pub id: Option<String>,
}

/// A single subtitle/caption track entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleTrack {
    /// Format extension yt-dlp would download for this track.
    pub ext: String,
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Top-level parsed `--dump-json` output.
///
/// `channel_title` is populated from whichever of yt-dlp's three
/// channel-name keys is present, in priority order:
/// `channel_title` → `channel` → `uploader`. Modern yt-dlp emits both
/// `channel` and `uploader` simultaneously (usually identical), so we
/// can't use serde's `alias` mechanism — it errors on duplicate keys.
/// Instead we deserialize via [`ExtractResultRaw`] and fold the three
/// keys together in [`From`].
#[derive(Debug, Clone, Serialize)]
pub struct ExtractResult {
    pub id: String,
    pub title: Option<String>,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub duration: Option<f64>,
    pub thumbnails: Vec<Thumbnail>,
    pub thumbnail: Option<String>,
    pub formats: Vec<Format>,
    /// User-uploaded subtitles, keyed by language code.
    pub subtitles: std::collections::HashMap<String, Vec<SubtitleTrack>>,
    /// Auto-generated captions, keyed by language code.
    pub automatic_captions: std::collections::HashMap<String, Vec<SubtitleTrack>>,
    /// SegmentBase byte ranges parsed from YouTube's innertube
    /// `/player` response (via yt-dlp `--write-pages`). Keyed by itag
    /// (the integer format identifier YouTube assigns). Each value
    /// provides the inclusive byte ranges for the initialization
    /// segment (moov / EBML header) and the segment index
    /// (sidx / Cues). These are used by the DASH synthesizer
    /// to emit `<SegmentBase indexRange>` + `<Initialization range>`.
    ///
    /// Populated at extract time by matching each format's `filesize`
    /// against the `contentLength` field in innertube's adaptiveFormats
    /// array. This per-format-id keying correctly handles dubbed audio
    /// tracks (different files sharing an itag but with different
    /// byte offsets).
    #[serde(default)]
    pub format_box_ranges: std::collections::HashMap<String, SegmentRanges>,
}

/// Inclusive byte ranges for a single adaptive format's SegmentBase,
/// as reported by YouTube's innertube `/player` API.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SegmentRanges {
    pub init_start: u64,
    pub init_end: u64,
    pub index_start: u64,
    pub index_end: u64,
}

/// Raw shape of yt-dlp's `--dump-json` output. Used purely as a
/// deserialization target; consumers see the canonicalised
/// [`ExtractResult`].
#[derive(Debug, Deserialize)]
struct ExtractResultRaw {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    channel_title: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    thumbnails: Vec<Thumbnail>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    formats: Vec<Format>,
    #[serde(default)]
    subtitles: std::collections::HashMap<String, Vec<SubtitleTrack>>,
    #[serde(default)]
    automatic_captions: std::collections::HashMap<String, Vec<SubtitleTrack>>,
    #[serde(default)]
    format_box_ranges: std::collections::HashMap<String, SegmentRanges>,
}

impl From<ExtractResultRaw> for ExtractResult {
    fn from(raw: ExtractResultRaw) -> Self {
        // Prefer the most canonical key. `channel_title` is the only
        // one that's guaranteed unambiguous when present; `channel` is
        // the modern default; `uploader` is the legacy fallback.
        let channel_title = raw.channel_title.or(raw.channel).or(raw.uploader);
        ExtractResult {
            id: raw.id,
            title: raw.title,
            channel_id: raw.channel_id,
            channel_title,
            duration: raw.duration,
            thumbnails: raw.thumbnails,
            thumbnail: raw.thumbnail,
            formats: raw.formats,
            subtitles: raw.subtitles,
            automatic_captions: raw.automatic_captions,
            format_box_ranges: raw.format_box_ranges,
        }
    }
}

impl<'de> Deserialize<'de> for ExtractResult {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        ExtractResultRaw::deserialize(deserializer).map(Into::into)
    }
}

/// Run `yt-dlp --dump-json --no-playlist <video_url>` and parse the
/// result. Times out after [`DEFAULT_TIMEOUT`].
///
/// Additionally runs with `--write-pages` to capture the raw innertube
/// `/player` API response, from which we extract `initRange` and
/// `indexRange` for each adaptive format. These byte ranges let us
/// emit `<SegmentBase indexRange>` + `<Initialization range>` in the
/// synthesized DASH manifest without probing the upstream files.
pub async fn extract(cfg: &Config, video_id: &str) -> AppResult<ExtractResult> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");

    // yt-dlp's --write-pages dumps all HTTP responses to the working
    // directory. Use a dedicated temp dir so we can find the player
    // dumps without polluting the project root.
    let pages_dir = tempdir_for_pages(video_id);
    tokio::fs::create_dir_all(&pages_dir)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("creating pages tmp: {e}")))?;

    let mut cmd = Command::new(&cfg.ytdlp_path);
    cmd.arg("--dump-json")
        .arg("--no-playlist")
        .arg("--no-warnings")
        .arg("--skip-download")
        .arg("--write-pages")
        .current_dir(&pages_dir);
    let yt_args_guard = append_youtube_args(&mut cmd);
    cmd.arg(&url);
    debug!(?cmd, %video_id, "running yt-dlp");

    let output = timeout(DEFAULT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| AppError::Other(anyhow::anyhow!("yt-dlp timed out after 30s")))?
        .map_err(|e| AppError::Other(anyhow::anyhow!("spawning yt-dlp: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(%video_id, %stderr, "yt-dlp failed");
        let _ = tokio::fs::remove_dir_all(&pages_dir).await;
        return Err(AppError::Other(anyhow::anyhow!(
            "yt-dlp exited with status {}: {}",
            output.status,
            stderr
        )));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| AppError::Other(anyhow::anyhow!("yt-dlp stdout not UTF-8: {e}")))?;
    let mut result: ExtractResult = serde_json::from_str(&stdout)
        .map_err(|e| AppError::Other(anyhow::anyhow!("parsing yt-dlp JSON: {e}")))?;

    // Parse SegmentBase ranges from the innertube player dump(s) and
    // resolve them per-format-id. Innertube's `adaptiveFormats` array
    // contains one entry per file variant — for itag 249 there may be
    // three entries (different language dubs), each with its own byte
    // ranges. We match against yt-dlp's per-format `filesize` using
    // innertube's `contentLength` to assign the right ranges to each
    // `format_id`.
    let innertube_ranges = parse_player_page_dumps(&pages_dir).await;
    if !innertube_ranges.is_empty() {
        // Build itag → [ranges] index to enable single-entry fallback
        // when a format has no `filesize`.
        let mut by_itag: std::collections::HashMap<i64, Vec<SegmentRanges>> =
            std::collections::HashMap::new();
        for ((itag, _cl), sr) in &innertube_ranges {
            by_itag.entry(*itag).or_default().push(*sr);
        }
        for f in &result.formats {
            let Some(itag) = parse_itag_from_format_id(&f.format_id) else {
                continue;
            };
            // Match by (itag, filesize) — the canonical path.
            let sr = if let Some(fs) = f.filesize.and_then(|s| u64::try_from(s).ok()) {
                innertube_ranges.get(&(itag, fs)).copied()
            } else {
                None
            };
            // Fallback: if the itag has exactly one innertube entry,
            // there's no variant ambiguity — use it.
            let sr = sr.or_else(|| {
                by_itag
                    .get(&itag)
                    .filter(|v| v.len() == 1)
                    .map(|v| v[0])
            });
            if let Some(sr) = sr {
                result.format_box_ranges.insert(f.format_id.clone(), sr);
            }
        }
        debug!(
            %video_id,
            innertube_entries = innertube_ranges.len(),
            resolved = result.format_box_ranges.len(),
            "resolved per-format-id segment ranges from innertube"
        );
    } else {
        debug!(%video_id, "no segment ranges found in player dumps");
    }

    // Cleanup pages temp dir (best-effort).
    let _ = tokio::fs::remove_dir_all(&pages_dir).await;

    // Fold any rotated session cookies (e.g. `__Secure-1PSIDTS`) back
    // into the canonical cookie file, but only if every original cookie
    // name still survived yt-dlp's cookiejar rewrite.
    yt_args_guard.persist_cookies_if_safe().await;
    Ok(result)
}

/// Temp directory for yt-dlp's `--write-pages` output.
fn tempdir_for_pages(video_id: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nonce: u64 = rand::random();
    p.push(format!("hometube-pages-{video_id}-{nonce:x}"));
    p
}

/// Scan a directory of yt-dlp `--write-pages` dumps for innertube
/// `/player` responses and extract `initRange` / `indexRange` for
/// each adaptive format.
///
/// Returns a map from itag (YouTube's integer format identifier) to
/// the inclusive byte ranges for the initialization segment and the
/// segment index. Missing or malformed entries are silently skipped.
/// Extract the itag (integer prefix) from a yt-dlp format identifier.
///
/// yt-dlp names formats like `"303"`, `"303-dashy"`, `"251-drc"`,
/// `"251-0"`, `"251-dashy-1"`. The itag is always the leading integer
/// prefix before the first `-` (or the entire string if no `-`).
pub fn parse_itag_from_format_id(format_id: &str) -> Option<i64> {
    let numeric_prefix: String = format_id
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    numeric_prefix.parse::<i64>().ok()
}

/// Scan yt-dlp's `--write-pages` dumps for innertube `/player`
/// responses and extract `initRange` / `indexRange` for each adaptive
/// format entry.
///
/// Innertube's `adaptiveFormats` array contains *one entry per file
/// variant* — multiple entries for itag 249 represent different
/// language dubs, each with its own byte ranges. We key the returned
/// map by `(itag, contentLength)` so the caller can disambiguate
/// variants by matching against yt-dlp's per-format `filesize`.
///
/// Entries missing any of itag / contentLength / initRange / indexRange
/// are silently skipped.
async fn parse_player_page_dumps(
    dir: &std::path::Path,
) -> std::collections::HashMap<(i64, u64), SegmentRanges> {
    let mut ranges = std::collections::HashMap::new();

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return ranges,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // yt-dlp names player API dumps with "youtubei_v1_player" in
        // the filename.
        if !name.contains("youtubei_v1_player") {
            continue;
        }
        let body = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let json: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let formats = json
            .get("streamingData")
            .and_then(|sd| sd.get("adaptiveFormats"))
            .and_then(|af| af.as_array());
        let Some(formats) = formats else { continue };
        for fmt in formats {
            let Some(itag) = fmt.get("itag").and_then(|v| v.as_i64()) else {
                continue;
            };
            // `contentLength` is a stringified integer in innertube's
            // response. Used to match the variant against yt-dlp's
            // `filesize` field.
            let Some(content_length) = fmt
                .get("contentLength")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
            else {
                continue;
            };
            let init_range = fmt.get("initRange");
            let index_range = fmt.get("indexRange");
            let (Some(init_range), Some(index_range)) = (init_range, index_range) else {
                continue;
            };
            let parse_pair = |obj: &serde_json::Value| -> Option<(u64, u64)> {
                let s = obj
                    .get("start")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())?;
                let e = obj
                    .get("end")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())?;
                Some((s, e))
            };
            let Some((init_start, init_end)) = parse_pair(init_range) else {
                continue;
            };
            let Some((index_start, index_end)) = parse_pair(index_range) else {
                continue;
            };
            ranges.insert(
                (itag, content_length),
                SegmentRanges {
                    init_start,
                    init_end,
                    index_start,
                    index_end,
                },
            );
        }
    }
    ranges
}

/// Run yt-dlp with `--write-sub --convert-subs vtt --skip-download` for a
/// single language and return the resulting WebVTT body.
///
/// Used by the Phase 16 caption-serve route when the upstream caption
/// track is something other than WebVTT (typically SRV1/SRV3/TTML for
/// auto-captions). yt-dlp performs the conversion via ffmpeg's subtitle
/// muxer; the resulting `.vtt` file is read back into memory and the
/// temp directory is removed.
pub async fn extract_subtitles(cfg: &Config, video_id: &str, lang: &str) -> AppResult<String> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let tmp = tempdir_for_video(video_id);
    tokio::fs::create_dir_all(&tmp)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("creating subtitle tmp: {e}")))?;
    let template = tmp.join("%(id)s").to_string_lossy().to_string();

    let mut cmd = Command::new(&cfg.ytdlp_path);
    cmd.arg("--write-sub")
        .arg("--write-auto-sub")
        .arg("--sub-lang")
        .arg(lang)
        .arg("--skip-download")
        .arg("--convert-subs")
        .arg("vtt")
        .arg("--no-warnings")
        .arg("-o")
        .arg(&template);
    let yt_args_guard = append_youtube_args(&mut cmd);
    cmd.arg(&url);
    debug!(?cmd, %video_id, %lang, "running yt-dlp for subtitles");

    let output = timeout(DEFAULT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| AppError::Other(anyhow::anyhow!("yt-dlp timed out after 30s")))?
        .map_err(|e| AppError::Other(anyhow::anyhow!("spawning yt-dlp: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(%video_id, %lang, %stderr, "yt-dlp subtitle extraction failed");
        // Cleanup temp directory before returning.
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(AppError::Other(anyhow::anyhow!(
            "yt-dlp exited with status {}: {}",
            output.status,
            stderr
        )));
    }

    // The output filename should be `<video_id>.<lang>.vtt`.
    let expected = tmp.join(format!("{video_id}.{lang}.vtt"));
    let body = match tokio::fs::read_to_string(&expected).await {
        Ok(s) => s,
        Err(_) => {
            // Fall back: scan the directory for any .vtt file.
            let mut found: Option<String> = None;
            if let Ok(mut rd) = tokio::fs::read_dir(&tmp).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("vtt") {
                        if let Ok(s) = tokio::fs::read_to_string(&path).await {
                            found = Some(s);
                            break;
                        }
                    }
                }
            }
            match found {
                Some(s) => s,
                None => {
                    let _ = tokio::fs::remove_dir_all(&tmp).await;
                    return Err(AppError::Other(anyhow::anyhow!(
                        "yt-dlp produced no .vtt for {video_id}/{lang}"
                    )));
                }
            }
        }
    };

    let _ = tokio::fs::remove_dir_all(&tmp).await;
    // Fold any rotated session cookies back into the canonical file
    // (gated on the survivor check inside the guard).
    yt_args_guard.persist_cookies_if_safe().await;
    Ok(body)
}

/// Returns the path where the yt-dlp cookies file is stored on disk.
/// Configurable via `YTDLP_COOKIES_PATH` env var (default: `/data/cookies.txt`).
pub fn cookies_file_path() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("YTDLP_COOKIES_PATH").unwrap_or_else(|_| "/data/cookies.txt".to_string()),
    )
}

/// Write cookie content to the deterministic cookies file path.
/// If `content` is `None` or empty, removes the file instead.
pub fn sync_cookies_to_disk(content: Option<&str>) -> std::io::Result<()> {
    let path = cookies_file_path();
    match content {
        Some(c) if !c.trim().is_empty() => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, c)?;
            // Restrict permissions to owner-only on Unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        _ => {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Guard returned by [`append_youtube_args`] that owns any per-invocation
/// temp files (currently just the throwaway cookies copy). Drop it
/// *after* `cmd.output().await` so yt-dlp can finish reading the file.
///
/// On a successful run, call [`Self::persist_cookies_if_safe`] before
/// dropping to fold yt-dlp's refreshed session cookies (e.g.
/// `__Secure-1PSIDTS`, `SIDCC`) back into the canonical cookies file.
/// The persist step is gated on a survivor check — if any of the cookies
/// captured at startup are missing from the rewritten jar, we keep the
/// original instead of letting yt-dlp's cookiejar pruning erode auth.
pub struct YoutubeArgsGuard {
    cookies_tempfile: Option<std::path::PathBuf>,
    /// Snapshot of cookie names present in the file we passed to
    /// yt-dlp (i.e. read from the tempfile copy itself, not the
    /// canonical source). Used as the survivor list for persistence.
    original_cookie_names: std::collections::HashSet<String>,
    /// Path of the canonical cookies file (where we'd persist back to).
    canonical_cookies_path: std::path::PathBuf,
}

impl YoutubeArgsGuard {
    /// Read yt-dlp's rewritten cookies tempfile and, if every cookie
    /// name we started with is still present, write the rewritten
    /// content back to the canonical cookies file. This captures the
    /// freshness benefit of yt-dlp's auto-rotation of session cookies
    /// (`__Secure-1PSIDTS`, `SIDCC`, etc.) while skipping rewrites that
    /// would drop auth cookies.
    ///
    /// Each invocation writes to its own per-call staging filename
    /// (`cookies.txt.new.<nonce>`) so concurrent extractions cannot
    /// stomp on each other's staged content before the atomic rename.
    ///
    /// Errors and "auth cookie missing" cases are logged at WARN and
    /// the canonical file is left untouched.
    pub async fn persist_cookies_if_safe(&self) {
        let Some(temp) = self.cookies_tempfile.as_ref() else {
            return;
        };
        let new_content = match tokio::fs::read_to_string(temp).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to read rewritten cookies tempfile");
                return;
            }
        };
        let new_names = parse_cookie_names(&new_content);
        let missing: Vec<&str> = self
            .original_cookie_names
            .iter()
            .filter(|n| !new_names.contains(n.as_str()))
            .map(|s| s.as_str())
            .collect();
        if !missing.is_empty() {
            warn!(
                missing = ?missing,
                "yt-dlp dropped cookies during rewrite; keeping canonical file unchanged"
            );
            return;
        }
        // All original cookie names survived; persist the refreshed
        // jar. Stage to a unique-per-invocation filename so concurrent
        // runs cannot overwrite each other's staged content, then
        // atomically rename onto the canonical path.
        let dest = &self.canonical_cookies_path;
        let nonce: u64 = rand::random();
        let staging = dest.with_extension(format!("txt.new.{nonce:x}"));
        if let Err(e) = tokio::fs::write(&staging, &new_content).await {
            warn!(error = %e, "failed to write staged cookies file");
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                tokio::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600)).await;
        }
        if let Err(e) = tokio::fs::rename(&staging, dest).await {
            warn!(error = %e, "failed to atomically replace canonical cookies file");
            let _ = tokio::fs::remove_file(&staging).await;
            return;
        }
        debug!("persisted refreshed cookies from yt-dlp run");
    }
}

impl Drop for YoutubeArgsGuard {
    fn drop(&mut self) {
        if let Some(p) = self.cookies_tempfile.take() {
            // Best-effort cleanup; ignore errors (file may already be
            // gone if yt-dlp deleted it, or we're shutting down). This
            // runs in `drop` which can't be async, so a sync remove is
            // unavoidable here — the file is small (<10 KB) and on the
            // tempdir's filesystem, so the call is effectively free.
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Extract cookie names from a Netscape/Mozilla cookies.txt body. Each
/// non-comment, non-blank line has 7 tab-separated fields, with the
/// cookie name in the 6th column.
fn parse_cookie_names(body: &str) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for line in body.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 7 {
            names.insert(fields[5].to_string());
        }
    }
    names
}

/// Append PO token arguments to a yt-dlp command:
///
/// 1. `--plugin-dirs <path>` — if the bgutil PO token plugin is installed.
/// 2. `--extractor-args youtube-bgutilhttp:base_url=<url>` — PO token
///    server URL (via `POT_SERVER_URL` env var, defaults to the Docker
///    Compose sidecar at `http://pot-server:4416`).
/// 3. `--cookies <path>` — if a cookies file exists on disk. The file
///    is *copied* to a tempfile first because yt-dlp rewrites the
///    cookie jar in place after each run, only persisting the cookies
///    it actually used. That gradually erodes the canonical cookie
///    set (auth cookies disappear after a few invocations) and breaks
///    authentication. The tempfile is owned by the returned guard.
/// 4. `--js-runtimes node` — yt-dlp needs a JS runtime to decode
///    YouTube's signature cipher; configurable via the
///    `YTDLP_JS_RUNTIME` env var (defaults to `node`).
fn append_youtube_args(cmd: &mut Command) -> YoutubeArgsGuard {
    // PO token plugin directory. Must be absolute because the caller
    // may set `.current_dir()` to a temp directory for `--write-pages`.
    let plugin_dir = std::env::var("YTDLP_PLUGIN_DIR")
        .unwrap_or_else(|_| "/usr/local/share/yt-dlp-plugins".to_string());
    let plugin_path = std::path::Path::new(&plugin_dir);
    let plugin_path_abs = if plugin_path.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(plugin_path))
            .unwrap_or_else(|_| plugin_path.to_path_buf())
    } else {
        plugin_path.to_path_buf()
    };
    if plugin_path_abs.exists() {
        cmd.arg("--plugin-dirs").arg(&plugin_path_abs);
    }

    // PO token server URL for the bgutil plugin.
    let pot_url =
        std::env::var("POT_SERVER_URL").unwrap_or_else(|_| "http://pot-server:4416".to_string());
    if !pot_url.is_empty() {
        cmd.arg("--extractor-args")
            .arg(format!("youtube-bgutilhttp:base_url={pot_url}"));
    }

    // YouTube extractor tuning. We deliberately request multiple player
    // clients so yt-dlp returns the *union* of formats they expose:
    //
    // - `default` keeps yt-dlp's built-in client list as a baseline.
    // - `ios` returns DASH-segmented formats *without* requiring a PO
    //   token — those URLs are the most reliable path for playback
    //   because Google's CDN doesn't 403 them the way it 403s
    //   PoT-pipelined HLS segments.
    // - `web` is the canonical client; it surfaces DASH manifests when
    //   it can authenticate via cookies + the bgutil PoT plugin, and
    //   gives us the richest format pool.
    //
    // `formats=duplicate` asks yt-dlp to keep adaptive *and*
    // progressive variants in the output, even when they overlap. That
    // gives `synthesize_manifest` more `https`-protocol formats to
    // pick from when no upstream DASH manifest is available, and lets
    // the rewriter prefer real DASH when it is.
    //
    // Configurable via the `YTDLP_PLAYER_CLIENT` env var so production
    // deployments can pin to a single client if they discover one
    // works better for their cookie set / IP geolocation.
    let player_clients =
        std::env::var("YTDLP_PLAYER_CLIENT").unwrap_or_else(|_| "default,ios,web".to_string());
    cmd.arg("--extractor-args").arg(format!(
        "youtube:player_client={player_clients};formats=duplicate"
    ));

    // Cookies file: copy to a tempfile so yt-dlp's in-place rewrite
    // doesn't erode the canonical jar. Snapshot the cookie names from
    // the *tempfile we just wrote* (not the canonical source) so the
    // survivor-check is consistent with what yt-dlp will actually see,
    // even if the canonical file is mutated concurrently.
    let mut cookies_tempfile: Option<std::path::PathBuf> = None;
    let mut original_cookie_names = std::collections::HashSet::new();
    let cookies_path = cookies_file_path();
    if cookies_path.exists() {
        match copy_cookies_to_tempfile(&cookies_path) {
            Ok(temp) => {
                if let Ok(body) = std::fs::read_to_string(&temp) {
                    original_cookie_names = parse_cookie_names(&body);
                }
                cmd.arg("--cookies").arg(&temp);
                cookies_tempfile = Some(temp);
            }
            Err(e) => {
                warn!(error = %e, "failed to copy cookies to tempfile; running without --cookies");
            }
        }
    }

    // JS runtime for YouTube signature cipher decoding. yt-dlp's default
    // (`deno`) is broken on some systems, and YouTube extraction without
    // a JS runtime has been deprecated upstream.
    let js_runtime = std::env::var("YTDLP_JS_RUNTIME").unwrap_or_else(|_| "node".to_string());
    if !js_runtime.is_empty() {
        cmd.arg("--js-runtimes").arg(&js_runtime);
    }

    YoutubeArgsGuard {
        cookies_tempfile,
        original_cookie_names,
        canonical_cookies_path: cookies_path,
    }
}

/// Copy the canonical cookies file to a unique tempfile and return the
/// new path. Permissions are restricted to the current user on Unix.
fn copy_cookies_to_tempfile(src: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let nonce: u64 = rand::random();
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("hometube-ytdlp-cookies-{nonce:x}.txt"));
    std::fs::copy(src, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(tmp)
}

fn tempdir_for_video(video_id: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nonce: u64 = rand::random();
    p.push(format!("hometube-subs-{video_id}-{nonce:x}"));
    p
}

/// Return the version string emitted by `yt-dlp --version`. Used by the
/// Phase 12 update job and the system status card.
pub async fn version(cfg: &Config) -> AppResult<String> {
    let output = timeout(
        Duration::from_secs(5),
        Command::new(&cfg.ytdlp_path).arg("--version").output(),
    )
    .await
    .map_err(|_| AppError::Other(anyhow::anyhow!("yt-dlp --version timed out")))?
    .map_err(|e| AppError::Other(anyhow::anyhow!("spawning yt-dlp: {e}")))?;
    if !output.status.success() {
        return Err(AppError::Other(anyhow::anyhow!(
            "yt-dlp --version failed with status {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Update flow
// ---------------------------------------------------------------------------

/// GitHub releases API endpoint for the yt-dlp project.
const GITHUB_LATEST_URL: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";

/// Direct download URL for the Linux static binary.
const LINUX_BINARY_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp";

/// Lookup the latest published version on GitHub. Returns the
/// `tag_name` field of the latest-release JSON.
pub async fn latest_published_version() -> AppResult<String> {
    let client = reqwest::Client::builder()
        .user_agent("hometube/0.1")
        .build()
        .map_err(AppError::Http)?;
    let res = client
        .get(GITHUB_LATEST_URL)
        .send()
        .await
        .map_err(AppError::Http)?;
    if !res.status().is_success() {
        return Err(AppError::Other(anyhow::anyhow!(
            "GitHub API returned {}",
            res.status()
        )));
    }
    let body: serde_json::Value = res.json().await.map_err(AppError::Http)?;
    body.get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("missing tag_name in GitHub response")))
}

/// Check whether a newer version is available compared to the value
/// stored in `ytdlp_info.current_version`. Returns `None` if already
/// up to date or no current_version is recorded.
pub async fn check_for_update(pool: &sqlx::SqlitePool) -> AppResult<Option<String>> {
    let latest = latest_published_version().await?;
    let current: Option<String> =
        sqlx::query_scalar("SELECT current_version FROM ytdlp_info WHERE id = 1")
            .fetch_optional(pool)
            .await?
            .flatten();
    sqlx::query("UPDATE ytdlp_info SET last_checked_at = unixepoch() WHERE id = 1")
        .execute(pool)
        .await
        .ok();
    if let Some(cur) = current {
        if cur.trim_start_matches('v') == latest.trim_start_matches('v') {
            return Ok(None);
        }
    }
    Ok(Some(latest))
}

/// Download and install the latest yt-dlp binary. Returns the new
/// version string on success (or the existing version if already up to
/// date). On any failure the existing binary is left untouched.
///
/// Implementation:
///   1. Use [`check_for_update`] to compare GitHub's latest tag with the
///      `current_version` column. If they match, return early — no
///      download necessary.
///   2. Download to `<binary_path>.new`.
///   3. `chmod +x`.
///   4. Run `<binary_path>.new --version` to verify it actually works.
///   5. Atomically rename to `<binary_path>` (replacing the old one).
///
/// Best-effort: if step 4 fails the temp file is removed so we don't
/// leave half-downloaded binaries lying around.
pub async fn update_binary(pool: &sqlx::SqlitePool, cfg: &Config) -> AppResult<String> {
    use tokio::fs;
    use tokio::io::AsyncWriteExt;

    // Skip the download entirely if we're already on the latest tag.
    // [`check_for_update`] also touches `last_checked_at` on the
    // `ytdlp_info` row.
    if check_for_update(pool).await?.is_none() {
        let current: Option<String> =
            sqlx::query_scalar("SELECT current_version FROM ytdlp_info WHERE id = 1")
                .fetch_optional(pool)
                .await?
                .flatten();
        return Ok(current.unwrap_or_else(|| "unknown".to_string()));
    }

    // Resolve target path. The configured path may be a bare command
    // name (e.g. `yt-dlp`) on first boot — in that case we install into
    // the data dir alongside `app.db`.
    let mut target = std::path::PathBuf::from(&cfg.ytdlp_path);
    if !target.is_absolute() && !target.exists() {
        target = std::path::PathBuf::from("./data/yt-dlp");
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await.ok();
        }
    }
    let temp = target.with_extension("new");

    // Download.
    let client = reqwest::Client::builder()
        .user_agent("hometube/0.1")
        .build()
        .map_err(AppError::Http)?;
    let res = client
        .get(LINUX_BINARY_URL)
        .send()
        .await
        .map_err(AppError::Http)?;
    if !res.status().is_success() {
        return Err(AppError::Other(anyhow::anyhow!(
            "yt-dlp download returned HTTP {}",
            res.status()
        )));
    }
    let bytes = res.bytes().await.map_err(AppError::Http)?;
    let mut f = fs::File::create(&temp)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("creating temp file: {e}")))?;
    f.write_all(&bytes)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("writing temp file: {e}")))?;
    f.flush().await.ok();
    drop(f);

    // chmod +x (Unix only — on Windows we no-op since the OS doesn't
    // care about the executable bit).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&temp)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("statting temp: {e}")))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&temp, perms)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("chmod temp: {e}")))?;
    }

    // Verify.
    let verify = timeout(
        Duration::from_secs(10),
        Command::new(&temp).arg("--version").output(),
    )
    .await
    .map_err(|_| AppError::Other(anyhow::anyhow!("yt-dlp --version timed out")))
    .and_then(|res| res.map_err(|e| AppError::Other(anyhow::anyhow!("spawn: {e}"))));

    let verify = match verify {
        Ok(v) => v,
        Err(err) => {
            fs::remove_file(&temp).await.ok();
            return Err(err);
        }
    };
    if !verify.status.success() {
        fs::remove_file(&temp).await.ok();
        return Err(AppError::Other(anyhow::anyhow!(
            "verification failed: status {}",
            verify.status
        )));
    }
    let new_version = String::from_utf8_lossy(&verify.stdout).trim().to_string();

    // Atomically replace.
    if let Err(err) = fs::rename(&temp, &target).await {
        fs::remove_file(&temp).await.ok();
        return Err(AppError::Other(anyhow::anyhow!(
            "renaming new binary into place: {err}"
        )));
    }

    // Persist version metadata.
    let target_str = target.to_string_lossy().to_string();
    sqlx::query(
        "INSERT INTO ytdlp_info (id, current_version, last_checked_at, last_updated_at, binary_path) \
         VALUES (1, ?, unixepoch(), unixepoch(), ?) \
         ON CONFLICT(id) DO UPDATE SET \
            current_version = excluded.current_version, \
            last_checked_at = excluded.last_checked_at, \
            last_updated_at = excluded.last_updated_at, \
            binary_path = excluded.binary_path",
    )
    .bind(&new_version)
    .bind(&target_str)
    .execute(pool)
    .await?;

    Ok(new_version)
}
