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
        .arg("--skip-download");
    append_youtube_args(&mut cmd);
    cmd.arg(&url);
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
    append_youtube_args(&mut cmd);
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

/// Append PO token arguments to a yt-dlp command:
///
/// 1. `--plugin-dirs <path>` — if the bgutil PO token plugin is installed.
/// 2. `--extractor-args youtube-bgutilhttp:base_url=<url>` — PO token
///    server URL (via `POT_SERVER_URL` env var, defaults to the Docker
///    Compose sidecar at `http://pot-server:4416`).
/// 3. `--cookies <path>` — if a cookies file exists on disk.
fn append_youtube_args(cmd: &mut Command) {
    // PO token plugin directory.
    let plugin_dir = std::env::var("YTDLP_PLUGIN_DIR")
        .unwrap_or_else(|_| "/usr/local/share/yt-dlp-plugins".to_string());
    if std::path::Path::new(&plugin_dir).exists() {
        cmd.arg("--plugin-dirs").arg(&plugin_dir);
    }

    // PO token server URL for the bgutil plugin.
    let pot_url =
        std::env::var("POT_SERVER_URL").unwrap_or_else(|_| "http://pot-server:4416".to_string());
    if !pot_url.is_empty() {
        cmd.arg("--extractor-args")
            .arg(format!("youtube-bgutilhttp:base_url={pot_url}"));
    }

    // Cookies file for authenticated YouTube access.
    let cookies_path = cookies_file_path();
    if cookies_path.exists() {
        cmd.arg("--cookies").arg(&cookies_path);
    }
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
