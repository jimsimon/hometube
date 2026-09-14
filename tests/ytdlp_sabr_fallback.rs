//! Integration test for `ytdlp::extract`'s SABR-only fallback.
//!
//! Wires `Config::ytdlp_path` at a shell shim that behaves like
//! yt-dlp does when YouTube forces SABR streaming on a logged-in
//! session: with `--cookies` it emits only HLS formats (no direct-URL
//! `https` formats) plus the real-world warning on stderr; without
//! `--cookies` it emits a normal adaptive format list. Each invocation
//! appends its argv to a log file so we can assert on the retry.
//!
//! Lives in its own test binary because it sets `YTDLP_COOKIES_PATH`
//! process-wide.

use std::io::Write;

use hometube::config::Config;
use hometube::services::ytdlp;

const SABR_ONLY_JSON: &str = r#"{"id":"vid-1","title":"SABR","duration":10.0,"formats":[
  {"format_id":"sb0","protocol":"mhtml","url":"https://i.ytimg.com/sb/x"},
  {"format_id":"233","protocol":"m3u8_native","acodec":"mp4a.40.5","vcodec":"none","url":"https://manifest.googlevideo.com/hls/233.m3u8"},
  {"format_id":"270","protocol":"m3u8_native","acodec":"none","vcodec":"avc1.640028","height":1080,"url":"https://manifest.googlevideo.com/hls/270.m3u8"}
]}"#;

/// What a SABR-only *logged-in* session actually returns per yt-dlp
/// #17666: everything adaptive is gone, but the muxed 360p format 18
/// still comes back with a direct URL. The DASH synthesizer rejects
/// muxed formats, so this must still count as "nothing playable".
const SABR_WITH_FORMAT_18_JSON: &str = r#"{"id":"vid-1","title":"SABR+18","duration":10.0,"formats":[
  {"format_id":"sb0","protocol":"mhtml","url":"https://i.ytimg.com/sb/x"},
  {"format_id":"18","protocol":"https","acodec":"mp4a.40.2","vcodec":"avc1.42001E","height":360,"filesize":1000,"url":"https://rr1.googlevideo.com/videoplayback?itag=18","format_note":"360p, MWEB"},
  {"format_id":"233","protocol":"m3u8_native","acodec":"mp4a.40.5","vcodec":"none","url":"https://manifest.googlevideo.com/hls/233.m3u8"}
]}"#;

/// A healthy result: direct adaptive formats *with* the innertube
/// `<SegmentBase>` ranges the synthesizer needs. (In production these
/// come from `--write-pages` dumps; the shim can't produce those, so
/// they ride along in the JSON via the `#[serde(default)]` field.)
const DIRECT_JSON: &str = r#"{"id":"vid-1","title":"Direct","duration":10.0,"formats":[
  {"format_id":"sb0","protocol":"mhtml","url":"https://i.ytimg.com/sb/x"},
  {"format_id":"251","protocol":"https","acodec":"opus","vcodec":"none","filesize":100,"url":"https://rr1.googlevideo.com/videoplayback?itag=251"},
  {"format_id":"303","protocol":"https","acodec":"none","vcodec":"vp9","height":1080,"filesize":200,"url":"https://rr1.googlevideo.com/videoplayback?itag=303"}
],"format_box_ranges":{
  "251":{"init_start":0,"init_end":99,"index_start":100,"index_end":199},
  "303":{"init_start":0,"init_end":99,"index_start":100,"index_end":199}
}}"#;

/// Same direct formats, but no ranges resolved. `synthesize_manifest`
/// drops every unranged Representation, so this is just as
/// unplayable as the SABR-only shape and must trigger the retry.
const DIRECT_UNRANGED_JSON: &str = r#"{"id":"vid-1","title":"Unranged","duration":10.0,"formats":[
  {"format_id":"sb0","protocol":"mhtml","url":"https://i.ytimg.com/sb/x"},
  {"format_id":"251","protocol":"https","acodec":"opus","vcodec":"none","filesize":100,"url":"https://rr1.googlevideo.com/videoplayback?itag=251"},
  {"format_id":"303","protocol":"https","acodec":"none","vcodec":"vp9","height":1080,"filesize":200,"url":"https://rr1.googlevideo.com/videoplayback?itag=303"}
]}"#;

fn write_shim(
    argv_log: &std::path::Path,
    with_cookies_json: &str,
    no_cookies_json: &str,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let nonce: u64 = rand::random();
    let mut path = std::env::temp_dir();
    path.push(format!("hometube-ytdlp-sabr-shim-{nonce:x}.sh"));

    let esc = |s: &str| s.replace('\'', r#"'\''"#);
    let tmp_path = path.with_extension("sh.partial");
    {
        let mut f = std::fs::File::create(&tmp_path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "printf '%s\\n' \"$*\" >> '{}'", argv_log.display()).unwrap();
        writeln!(f, "case \" $* \" in").unwrap();
        writeln!(f, "  *' --cookies '*)").unwrap();
        writeln!(
            f,
            "    printf '%s\\n' 'WARNING: [youtube] vid-1: Some web_embedded client https formats have been skipped as they are missing a URL. YouTube may have enabled the SABR-only streaming experiment for the current session.' >&2"
        )
        .unwrap();
        writeln!(f, "    printf '%s\\n' '{}' ;;", esc(with_cookies_json)).unwrap();
        writeln!(f, "  *)").unwrap();
        writeln!(f, "    printf '%s\\n' '{}' ;;", esc(no_cookies_json)).unwrap();
        writeln!(f, "esac").unwrap();
        writeln!(f, "exit 0").unwrap();
        f.flush().unwrap();
    }
    let mut perms = std::fs::metadata(&tmp_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tmp_path, perms).unwrap();
    std::fs::rename(&tmp_path, &path).unwrap();
    path
}

fn config_with_ytdlp(path: &std::path::Path) -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 0,
        database_url: "sqlite::memory:".into(),
        ytdlp_path: path.to_string_lossy().into_owned(),
        static_dir: "./frontend/dist".into(),
        cache_dir: "./data/cache".into(),
    }
}

/// `YTDLP_COOKIES_PATH` is process-wide; the tests in this binary run
/// on separate threads, so serialize them.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    _env: std::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
    argv_log: std::path::PathBuf,
    shim: std::path::PathBuf,
}

impl Fixture {
    fn new(with_cookies_json: &str, no_cookies_json: &str) -> Self {
        let env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let nonce: u64 = rand::random();
        let dir = std::env::temp_dir().join(format!("hometube-sabr-fallback-{nonce:x}"));
        std::fs::create_dir_all(&dir).unwrap();
        let cookies = dir.join("cookies.txt");
        std::fs::write(
            &cookies,
            "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tabc\n",
        )
        .unwrap();
        unsafe { std::env::set_var("YTDLP_COOKIES_PATH", cookies.to_str().unwrap()) };
        let argv_log = dir.join("argv.log");
        let shim = write_shim(&argv_log, with_cookies_json, no_cookies_json);
        Self {
            _env: env,
            dir,
            argv_log,
            shim,
        }
    }

    fn invocations(&self) -> Vec<String> {
        std::fs::read_to_string(&self.argv_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.shim);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn sabr_only_with_cookies_retries_without_cookies() {
    let fx = Fixture::new(SABR_ONLY_JSON, DIRECT_JSON);
    let cfg = config_with_ytdlp(&fx.shim);

    let result = ytdlp::extract(&cfg, "vid-1")
        .await
        .expect("extract succeeds");

    // The cookie-less retry's output won.
    assert_eq!(result.title.as_deref(), Some("Direct"));
    assert_eq!(result.usable_format_count(), 2);

    let calls = fx.invocations();
    assert_eq!(calls.len(), 2, "expected exactly one retry, got {calls:?}");
    assert!(
        calls[0].contains("--cookies"),
        "first attempt should pass cookies: {}",
        calls[0]
    );
    assert!(
        !calls[1].contains("--cookies"),
        "retry must not pass cookies: {}",
        calls[1]
    );
    // Warnings are no longer suppressed — they're our only diagnostic
    // for the SABR-only condition.
    assert!(!calls[0].contains("--no-warnings"));
    // The logged-in attempt asks for `web_creator` (the only cookie
    // client still serving direct formats); the logged-out retry
    // doesn't bother, since it would just be LOGIN_REQUIRED.
    assert!(
        calls[0].contains("web_creator"),
        "cookie run should request web_creator: {}",
        calls[0]
    );
    assert!(
        !calls[1].contains("web_creator"),
        "cookie-less retry should not request web_creator: {}",
        calls[1]
    );
}

#[tokio::test]
async fn muxed_only_cookie_result_still_retries_without_cookies() {
    let fx = Fixture::new(SABR_WITH_FORMAT_18_JSON, DIRECT_JSON);
    let cfg = config_with_ytdlp(&fx.shim);

    let result = ytdlp::extract(&cfg, "vid-1")
        .await
        .expect("extract succeeds");

    // Format 18 has a URL but is muxed, so it must not be mistaken for
    // a playable result: the retry fires and its output wins.
    assert_eq!(result.title.as_deref(), Some("Direct"));
    assert_eq!(result.usable_format_count(), 2);
    let calls = fx.invocations();
    assert_eq!(calls.len(), 2, "expected a retry, got {calls:?}");
    assert!(calls[0].contains("--cookies"));
    assert!(!calls[1].contains("--cookies"));
}

#[tokio::test]
async fn unranged_direct_cookie_result_still_retries_without_cookies() {
    let fx = Fixture::new(DIRECT_UNRANGED_JSON, DIRECT_JSON);
    let cfg = config_with_ytdlp(&fx.shim);

    let result = ytdlp::extract(&cfg, "vid-1")
        .await
        .expect("extract succeeds");

    // Direct URLs alone aren't playable: without SegmentBase ranges
    // the synthesizer emits nothing, so the retry must fire.
    assert_eq!(result.title.as_deref(), Some("Direct"));
    assert_eq!(result.usable_format_count(), 2);
    let calls = fx.invocations();
    assert_eq!(calls.len(), 2, "expected a retry, got {calls:?}");
    assert!(calls[0].contains("--cookies"));
    assert!(!calls[1].contains("--cookies"));
}

#[tokio::test]
async fn direct_formats_with_cookies_do_not_retry() {
    let fx = Fixture::new(DIRECT_JSON, SABR_ONLY_JSON);
    let cfg = config_with_ytdlp(&fx.shim);

    let result = ytdlp::extract(&cfg, "vid-1")
        .await
        .expect("extract succeeds");

    assert_eq!(result.title.as_deref(), Some("Direct"));
    assert_eq!(result.usable_format_count(), 2);
    let calls = fx.invocations();
    assert_eq!(calls.len(), 1, "no retry expected, got {calls:?}");
    assert!(calls[0].contains("--cookies"));
}

#[tokio::test]
async fn sabr_only_everywhere_keeps_cookie_result() {
    let fx = Fixture::new(SABR_ONLY_JSON, SABR_ONLY_JSON);
    let cfg = config_with_ytdlp(&fx.shim);

    let result = ytdlp::extract(&cfg, "vid-1")
        .await
        .expect("extract succeeds");

    // Still a successful extraction (metadata is useful for the
    // unavailable page), just nothing playable.
    assert_eq!(result.title.as_deref(), Some("SABR"));
    assert_eq!(result.usable_format_count(), 0);
    assert_eq!(fx.invocations().len(), 2);
}
