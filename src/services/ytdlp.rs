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
    /// DASH manifest URL (some formats only expose a manifest).
    #[serde(default)]
    pub manifest_url: Option<String>,
    /// `"https"`, `"http_dash_segments"`, etc.
    #[serde(default)]
    pub protocol: Option<String>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractResult {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default, alias = "channel", alias = "uploader")]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub thumbnails: Vec<Thumbnail>,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub formats: Vec<Format>,
    /// User-uploaded subtitles, keyed by language code.
    #[serde(default)]
    pub subtitles: std::collections::HashMap<String, Vec<SubtitleTrack>>,
    /// Auto-generated captions, keyed by language code.
    #[serde(default)]
    pub automatic_captions: std::collections::HashMap<String, Vec<SubtitleTrack>>,
    /// Some formats expose a single DASH manifest URL alongside the
    /// per-format URLs; yt-dlp also exposes it at top level.
    #[serde(default)]
    pub manifest_url: Option<String>,
}

/// Run `yt-dlp --dump-json --no-playlist <video_url>` and parse the
/// result. Times out after [`DEFAULT_TIMEOUT`].
pub async fn extract(cfg: &Config, video_id: &str) -> AppResult<ExtractResult> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let mut cmd = Command::new(&cfg.ytdlp_path);
    cmd.arg("--dump-json")
        .arg("--no-playlist")
        .arg("--no-warnings")
        .arg("--skip-download")
        .arg(&url);
    debug!(?cmd, %video_id, "running yt-dlp");

    let output = timeout(DEFAULT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| AppError::Other(anyhow::anyhow!("yt-dlp timed out after 30s")))?
        .map_err(|e| AppError::Other(anyhow::anyhow!("spawning yt-dlp: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(%video_id, %stderr, "yt-dlp failed");
        return Err(AppError::Other(anyhow::anyhow!(
            "yt-dlp exited with status {}: {}",
            output.status,
            stderr
        )));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| AppError::Other(anyhow::anyhow!("yt-dlp stdout not UTF-8: {e}")))?;
    let result: ExtractResult = serde_json::from_str(&stdout)
        .map_err(|e| AppError::Other(anyhow::anyhow!("parsing yt-dlp JSON: {e}")))?;
    Ok(result)
}

/// Return the version string emitted by `yt-dlp --version`. Used by the
/// Phase 12 update job and the system status card.
#[allow(dead_code)]
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
