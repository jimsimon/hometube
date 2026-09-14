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

/// How the shim behaves when invoked *without* `--cookies`.
#[derive(Clone, Copy)]
struct LoggedOut<'a> {
    /// JSON to print, or `None` to exit non-zero with a bot-wall message
    /// on stderr, like yt-dlp does when YouTube challenges an
    /// unauthenticated IP.
    json: Option<&'a str>,
    /// Seconds to sleep before answering, so a test can act mid-run.
    delay_secs: u32,
}

impl<'a> LoggedOut<'a> {
    /// A cookie-less run that prints `json` immediately and exits 0.
    fn json(json: &'a str) -> Self {
        Self {
            json: Some(json),
            delay_secs: 0,
        }
    }
}

fn write_shim(
    argv_log: &std::path::Path,
    with_cookies_json: &str,
    logged_out: LoggedOut<'_>,
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
        if logged_out.delay_secs > 0 {
            writeln!(f, "    sleep {}", logged_out.delay_secs).unwrap();
        }
        match logged_out.json {
            Some(json) => writeln!(f, "    printf '%s\\n' '{}' ;;", esc(json)).unwrap(),
            None => writeln!(
                f,
                "    printf '%s\\n' 'ERROR: [youtube] vid-1: Sign in to confirm you'\\''re not a bot.' >&2; exit 1 ;;"
            )
            .unwrap(),
        }
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
    /// Fixture whose cookie-less run prints `no_cookies_json` immediately.
    fn new(with_cookies_json: &str, no_cookies_json: &str) -> Self {
        Self::build(with_cookies_json, LoggedOut::json(no_cookies_json))
    }

    /// Logged-out runs fail (bot wall); cookie runs return `with_cookies_json`.
    fn new_with_failing_logged_out(with_cookies_json: &str) -> Self {
        Self::build(
            with_cookies_json,
            LoggedOut {
                json: None,
                delay_secs: 0,
            },
        )
    }

    /// Logged-out runs answer only after `delay_secs`.
    fn new_with_slow_logged_out(
        with_cookies_json: &str,
        no_cookies_json: &str,
        delay_secs: u32,
    ) -> Self {
        Self::build(
            with_cookies_json,
            LoggedOut {
                json: Some(no_cookies_json),
                delay_secs,
            },
        )
    }

    /// Fixture with full control over the cookie-less run's behaviour
    /// (output, exit code, delay). Starts from a clear SABR-only memo.
    fn build(with_cookies_json: &str, logged_out: LoggedOut<'_>) -> Self {
        let env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // The SABR-only verdict is process-wide state; start every test
        // from "no verdict" so ordering doesn't matter.
        ytdlp::forget_sabr_only_session();
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
        let shim = write_shim(&argv_log, with_cookies_json, logged_out);
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

/// The jar exists on disk but can't be staged into a tempfile (here:
/// it's a directory, so `fs::copy` fails). The run is then logged-out
/// in practice, so it must not request `web_creator`, and `extract`
/// must not follow up with a second, identical cookie-less attempt.
#[tokio::test]
async fn failed_cookie_staging_runs_logged_out_without_retry() {
    let fx = Fixture::new(SABR_ONLY_JSON, SABR_ONLY_JSON);
    let cookies = fx.dir.join("cookies.txt");
    std::fs::remove_file(&cookies).unwrap();
    std::fs::create_dir(&cookies).unwrap();
    let cfg = config_with_ytdlp(&fx.shim);

    let result = ytdlp::extract(&cfg, "vid-1")
        .await
        .expect("extract succeeds");

    assert_eq!(result.usable_format_count(), 0);
    let calls = fx.invocations();
    assert_eq!(calls.len(), 1, "no retry expected, got {calls:?}");
    assert!(
        !calls[0].contains("--cookies"),
        "must not pass --cookies when staging failed: {}",
        calls[0]
    );
    assert!(
        !calls[0].contains("web_creator"),
        "must not request web_creator without cookies: {}",
        calls[0]
    );
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

/// Once the cookie-less retry has recovered playable formats, later
/// extractions must not keep paying for the doomed cookie run: they go
/// straight to the cookie-less attempt.
#[tokio::test]
async fn sabr_only_verdict_skips_cookie_run_on_later_extractions() {
    let fx = Fixture::new(SABR_ONLY_JSON, DIRECT_JSON);
    let cfg = config_with_ytdlp(&fx.shim);

    let first = ytdlp::extract(&cfg, "vid-1").await.unwrap();
    assert_eq!(first.usable_format_count(), 2);
    assert_eq!(
        fx.invocations().len(),
        2,
        "first extraction pays for the probe"
    );

    let second = ytdlp::extract(&cfg, "vid-2").await.unwrap();
    assert_eq!(second.title.as_deref(), Some("Direct"));
    assert_eq!(second.usable_format_count(), 2);
    let calls = fx.invocations();
    assert_eq!(
        calls.len(),
        3,
        "second extraction must be a single run: {calls:?}"
    );
    assert!(
        !calls[2].contains("--cookies"),
        "remembered SABR-only session must skip cookies: {}",
        calls[2]
    );
    assert!(!calls[2].contains("web_creator"));

    // Replacing the jar (a fresh login) clears the verdict, so the
    // cookie-authenticated run is tried again.
    ytdlp::forget_sabr_only_session();
    ytdlp::extract(&cfg, "vid-3").await.unwrap();
    let calls = fx.invocations();
    assert_eq!(
        calls.len(),
        5,
        "after reset the probe runs again: {calls:?}"
    );
    assert!(calls[3].contains("--cookies"));
    assert!(!calls[4].contains("--cookies"));
}

/// A SABR-only cookie run whose cookie-less retry *also* finds nothing
/// playable says nothing about the session (the video itself may be
/// unavailable), so no verdict is recorded.
#[tokio::test]
async fn unplayable_everywhere_does_not_record_a_verdict() {
    let fx = Fixture::new(SABR_ONLY_JSON, SABR_ONLY_JSON);
    let cfg = config_with_ytdlp(&fx.shim);

    ytdlp::extract(&cfg, "vid-1").await.unwrap();
    ytdlp::extract(&cfg, "vid-2").await.unwrap();
    let calls = fx.invocations();
    assert_eq!(
        calls.len(),
        4,
        "both extractions probe with cookies: {calls:?}"
    );
    assert!(calls[2].contains("--cookies"));
}

/// With a verdict in place, a cookie-less run that comes back unplayable
/// must not be the end of it: the verdict is dropped and the cookie run
/// is tried once, and its result wins when it is playable.
#[tokio::test]
async fn memo_skipped_run_falls_back_to_cookies_when_unplayable() {
    // Cookies now serve formats again (SABR experiment lifted), while
    // the logged-out session does not.
    let fx = Fixture::new(DIRECT_JSON, SABR_ONLY_JSON);
    let cfg = config_with_ytdlp(&fx.shim);
    ytdlp::remember_sabr_only_session();

    let result = ytdlp::extract(&cfg, "vid-1").await.unwrap();
    assert_eq!(result.title.as_deref(), Some("Direct"));
    assert_eq!(result.usable_format_count(), 2);
    let calls = fx.invocations();
    assert_eq!(calls.len(), 2, "logged-out first, then cookies: {calls:?}");
    assert!(!calls[0].contains("--cookies"));
    assert!(calls[1].contains("--cookies"));

    // The verdict was dropped, so the next extraction probes with
    // cookies first again (and needs no retry: cookies are playable).
    ytdlp::extract(&cfg, "vid-2").await.unwrap();
    let calls = fx.invocations();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(calls[2].contains("--cookies"));
}

/// A bot-walled logged-out run (non-zero exit) is an error, not an
/// empty result; it must still trigger the cookie fallback rather than
/// surfacing the error for the rest of the memo window.
#[tokio::test]
async fn memo_skipped_run_falls_back_to_cookies_on_error() {
    let fx = Fixture::new_with_failing_logged_out(DIRECT_JSON);
    let cfg = config_with_ytdlp(&fx.shim);
    ytdlp::remember_sabr_only_session();

    let result = ytdlp::extract(&cfg, "vid-1")
        .await
        .expect("cookie fallback recovers from a bot-walled logged-out run");
    assert_eq!(result.title.as_deref(), Some("Direct"));
    assert_eq!(result.usable_format_count(), 2);
    let calls = fx.invocations();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(!calls[0].contains("--cookies"));
    assert!(calls[1].contains("--cookies"));
}

/// Both attempts unplayable: still a successful extraction (metadata
/// for the unavailable page), the logged-out run is not repeated, and
/// the verdict stays cleared.
#[tokio::test]
async fn memo_skipped_run_unplayable_everywhere_returns_metadata_once() {
    let fx = Fixture::new(SABR_WITH_FORMAT_18_JSON, SABR_ONLY_JSON);
    let cfg = config_with_ytdlp(&fx.shim);
    ytdlp::remember_sabr_only_session();

    let result = ytdlp::extract(&cfg, "vid-1").await.unwrap();
    assert_eq!(result.usable_format_count(), 0);
    // The logged-out result is kept when neither attempt is playable.
    assert_eq!(result.title.as_deref(), Some("SABR"));
    let calls = fx.invocations();
    assert_eq!(calls.len(), 2, "exactly one attempt each: {calls:?}");
    assert!(!calls[0].contains("--cookies"));
    assert!(calls[1].contains("--cookies"));

    // Verdict cleared: the next extraction is back on the normal path.
    ytdlp::extract(&cfg, "vid-2").await.unwrap();
    let calls = fx.invocations();
    assert!(calls[2].contains("--cookies"), "{calls:?}");
}

/// A verdict describes the jar the extraction *started* with. If a
/// parent uploads new cookies while the cookie-less retry is still
/// running, the old extraction must not record its verdict against the
/// new jar — otherwise the fresh login would be ignored for an hour.
#[tokio::test]
async fn cookie_change_during_extraction_discards_the_stale_verdict() {
    let fx = Fixture::new_with_slow_logged_out(SABR_ONLY_JSON, DIRECT_JSON, 1);
    let cfg = config_with_ytdlp(&fx.shim);

    let extraction = {
        let cfg = cfg.clone();
        tokio::spawn(async move { ytdlp::extract(&cfg, "vid-1").await })
    };
    // Wait until the cookie run is done and the slow cookie-less retry
    // has started, then "upload new cookies" (what `set_cookies` does).
    for _ in 0..200 {
        if fx.invocations().len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(fx.invocations().len(), 2, "retry should be in flight");
    ytdlp::forget_sabr_only_session();

    let result = extraction.await.unwrap().unwrap();
    assert_eq!(
        result.usable_format_count(),
        2,
        "the old extraction still succeeds"
    );

    // The next extraction must try the new jar rather than inherit the
    // stale SABR-only verdict.
    ytdlp::extract(&cfg, "vid-2").await.unwrap();
    let calls = fx.invocations();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(
        calls[2].contains("--cookies"),
        "fresh cookies must be tried after an upload: {}",
        calls[2]
    );
}
