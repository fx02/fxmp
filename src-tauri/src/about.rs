//! "About": the app's name, version, the commit it was built from and the build
//! date (embedded by build.rs), and the project page.

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::i18n;

/// The project page; the only non-lookup URL the app opens.
pub const PROJECT_URL: &str = "https://github.com/fx02/fxmp";
/// Short commit hash, or "unknown" when built without git.
pub const COMMIT: &str = env!("SMP_GIT_HASH");
/// UTC build date, `YYYY-MM-DD`.
pub const BUILD_DATE: &str = env!("SMP_BUILD_DATE");

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct About {
    pub name: String,
    pub version: String,
    pub commit: String,
    pub build_date: String,
    pub url: &'static str,
    /// "fxmp 0.1.0 (abc1234, 2026-10-25)": "Copy version", the tray.
    pub text: String,
}

/// "fxmp 0.1.0 (abc1234, 2026-10-25)".
pub fn version_text(name: &str, version: &str, commit: &str, date: &str) -> String {
    format!("{name} {version} ({commit}, {date})")
}

pub fn about(app: &AppHandle) -> About {
    let info = app.package_info();
    let version = info.version.to_string();
    About {
        text: version_text(&info.name, &version, COMMIT, BUILD_DATE),
        name: info.name.clone(),
        version,
        commit: COMMIT.into(),
        build_date: BUILD_DATE.into(),
        url: PROJECT_URL,
    }
}

/// Opens [`PROJECT_URL`] in the default browser.
pub fn open_project_page(app: &AppHandle) -> Result<(), String> {
    if !crate::lookup::is_allowed(PROJECT_URL) {
        return Err(i18n::t("err.lookup_nothing"));
    }
    app.opener()
        .open_url(PROJECT_URL, None::<&str>)
        .map_err(|e| {
            log::warn!("cannot open the browser: {e}");
            i18n::tf("err.open_browser", &[("error", &e.to_string())])
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_line_names_the_commit_and_the_date() {
        assert_eq!(
            version_text("fxmp", "0.1.0", "abc1234", "2026-10-25"),
            "fxmp 0.1.0 (abc1234, 2026-10-25)"
        );
    }

    #[test]
    fn build_info_is_embedded() {
        assert!(
            COMMIT == "unknown"
                || (COMMIT.len() >= 7 && COMMIT.chars().all(|c| c.is_ascii_hexdigit())),
            "{COMMIT}"
        );
        let d: Vec<&str> = BUILD_DATE.split('-').collect();
        assert_eq!(
            d.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [4, 2, 2],
            "{BUILD_DATE}"
        );
        assert!(BUILD_DATE.chars().all(|c| c.is_ascii_digit() || c == '-'));
    }

    #[test]
    fn the_project_page_is_allowed_but_nothing_else_on_its_host() {
        assert!(crate::lookup::is_allowed(PROJECT_URL));
        for url in [
            "https://github.com/",
            "https://github.com/fx02",
            "https://github.com/fx02/fxmp/releases",
            "https://github.com/fx02/fxmp-evil",
            "http://github.com/fx02/fxmp",
            "https://github.com:443/fx02/fxmp?x=1",
            "https://github.com/fx02/fxmp#x",
            "https://gist.github.com/fx02/fxmp",
        ] {
            assert!(!crate::lookup::is_allowed(url), "{url}");
        }
    }
}
