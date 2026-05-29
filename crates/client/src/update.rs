//! Notify-only update checker.
//!
//! On launch (and periodically) the client asks the GitHub Releases API
//! for the latest published release of [`REPO`], compares its `tag_name`
//! against this binary's own compiled version, and — when a newer one
//! exists — surfaces it in the UI. The only action offered is opening the
//! release page in the browser; the client never downloads or replaces
//! its own binary.
//!
//! In-app self-update is deliberately out of scope: the macOS artifact is
//! an unsigned `.app` bundle that can't be swapped safely without
//! Developer ID signing + notarization, and a "download + replace +
//! relaunch" path would only work on Windows/Linux — an asymmetry not
//! worth the complexity for now. Notify-only is uniform and safe
//! everywhere.
//!
//! Everything network-facing runs on a one-shot `std::thread` so neither
//! the egui UI thread nor the gRPC/audio tokio runtime is ever blocked.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// `owner/repo` whose GitHub Releases drive the update check. Single
/// source of truth so the API URL stays in sync with the project.
const REPO: &str = "toki-hq/toki";

/// How long between automatic re-checks while the app stays open. Six
/// hours is plenty for a desktop tool and keeps us well under GitHub's
/// unauthenticated API budget (60 requests/hour/IP).
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// This binary's compiled version (the workspace `Cargo.toml` `version`).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Minimal view of the GitHub `releases/latest` payload — we only read
/// the tag and the human-facing page URL. Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
}

/// Outcome of a successful check.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Latest published version, normalized (leading `v` stripped).
    pub latest: String,
    /// `true` when `latest` is strictly newer than [`current_version`].
    pub newer: bool,
    /// GitHub release page to send the user to for a manual download.
    pub html_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("update check failed: {0}")]
    Http(String),
    #[error("GitHub rate limit reached — try again later")]
    RateLimited,
    #[error("couldn't parse the release info: {0}")]
    Parse(String),
}

/// Where the check currently stands, for the UI to render.
#[derive(Debug, Clone, Default)]
pub enum UpdatePhase {
    /// No check has run yet this session.
    #[default]
    Idle,
    /// A check is in flight on the worker thread.
    Checking,
    /// The latest release is not newer than us.
    UpToDate,
    /// A newer release exists.
    Available(UpdateInfo),
    /// The last check failed (message is user-facing).
    Error(String),
}

#[derive(Debug, Default)]
pub struct UpdateState {
    pub phase: UpdatePhase,
    /// When the last check *completed* (success or failure). Drives the
    /// periodic re-check timer in the GUI loop.
    pub last_checked: Option<Instant>,
}

pub type UpdateShared = Arc<Mutex<UpdateState>>;

pub fn shared() -> UpdateShared {
    Arc::new(Mutex::new(UpdateState::default()))
}

/// Blocking GitHub Releases API call. Runs on a worker thread (see
/// [`spawn_check`]); never call this from the UI thread.
pub fn check_latest() -> Result<UpdateInfo, UpdateError> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    // GitHub requires a User-Agent; the API-version header pins the JSON
    // shape so a future API default can't surprise us.
    let user_agent = format!("toki-client/{}", current_version());
    let body = match ureq::get(&url)
        .set("User-Agent", &user_agent)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
    {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| UpdateError::Http(e.to_string()))?,
        // 403 from the unauthenticated API is almost always the hourly
        // rate limit; surface a friendly, distinct message.
        Err(ureq::Error::Status(403, _)) => return Err(UpdateError::RateLimited),
        Err(ureq::Error::Status(code, resp)) => {
            return Err(UpdateError::Http(format!(
                "HTTP {code} {}",
                resp.status_text()
            )));
        }
        Err(ureq::Error::Transport(t)) => return Err(UpdateError::Http(t.to_string())),
    };

    let release: GhRelease =
        serde_json::from_str(&body).map_err(|e| UpdateError::Parse(e.to_string()))?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    let newer = is_newer(&latest, current_version());
    Ok(UpdateInfo {
        latest,
        newer,
        html_url: release.html_url,
    })
}

/// Strict semver "is `latest` newer than `current`". Unparseable input
/// (either side) is treated conservatively as "not newer" so a weird tag
/// never nags the user.
fn is_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(latest),
        semver::Version::parse(current),
    ) {
        (Ok(l), Ok(c)) => l > c,
        _ => false,
    }
}

/// Kick off a check on a background thread, writing the result into
/// `shared` and asking egui to repaint when it finishes. No-op if a check
/// is already in flight.
pub fn spawn_check(shared: UpdateShared, ctx: egui::Context) {
    {
        let mut st = shared.lock().unwrap();
        if matches!(st.phase, UpdatePhase::Checking) {
            return;
        }
        st.phase = UpdatePhase::Checking;
    }
    ctx.request_repaint();

    std::thread::spawn(move || {
        let result = check_latest();
        {
            let mut st = shared.lock().unwrap();
            st.last_checked = Some(Instant::now());
            st.phase = match result {
                Ok(info) if info.newer => UpdatePhase::Available(info),
                Ok(_) => UpdatePhase::UpToDate,
                Err(e) => UpdatePhase::Error(e.to_string()),
            };
        }
        // Wake the UI so the new phase paints without waiting for the
        // next animation frame.
        ctx.request_repaint();
    });
}

/// Open `url` in the user's default browser. Best-effort: a failure is
/// logged rather than surfaced, since the URL is also shown in the UI for
/// manual copy.
pub fn open_release_page(url: &str) {
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(url).spawn();

    if let Err(e) = spawned {
        tracing::warn!(error = %e, url, "failed to open release page");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_detects_upgrades() {
        assert!(is_newer("0.1.5", "0.1.4"));
        assert!(is_newer("0.2.0", "0.1.4"));
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn newer_rejects_same_or_older_or_garbage() {
        assert!(!is_newer("0.1.4", "0.1.4"));
        assert!(!is_newer("0.1.3", "0.1.4"));
        assert!(!is_newer("not-a-version", "0.1.4"));
        assert!(!is_newer("0.1.5", "also-garbage"));
    }

    #[test]
    fn current_version_is_valid_semver() {
        assert!(semver::Version::parse(current_version()).is_ok());
    }

    #[test]
    fn parses_releases_fixture_and_strips_v_prefix() {
        // Trimmed sample of a real GitHub releases/latest response — extra
        // fields present to prove unknown keys are ignored.
        let body = r#"{
            "tag_name": "v0.9.1",
            "name": "Toki 0.9.1",
            "html_url": "https://github.com/toki-hq/toki/releases/tag/v0.9.1",
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": "toki-client-0.9.1-macos.zip", "browser_download_url": "https://example/x"}
            ]
        }"#;
        let rel: GhRelease = serde_json::from_str(body).unwrap();
        assert_eq!(rel.tag_name, "v0.9.1");
        let latest = rel.tag_name.trim_start_matches('v');
        assert_eq!(latest, "0.9.1");
        assert!(is_newer(latest, "0.1.4"));
        assert!(rel.html_url.ends_with("/releases/tag/v0.9.1"));
    }
}
