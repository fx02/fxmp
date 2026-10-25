use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    embed_build_info();
    tauri_build::build()
}

/// `SMP_GIT_HASH` (short commit, "unknown" without git, e.g. a source tarball) and
/// `SMP_BUILD_DATE` (UTC `YYYY-MM-DD`) for "About".
fn embed_build_info() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&manifest)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=SMP_GIT_HASH={hash}");
    // SOURCE_DATE_EPOCH (reproducible builds) wins over the clock.
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        });
    println!("cargo:rustc-env=SMP_BUILD_DATE={}", utc_date(secs));
    // A new commit (or checkout) rebuilds the info.
    if let Some(git) = git_dir(&manifest) {
        for f in ["HEAD", "packed-refs"] {
            println!("cargo:rerun-if-changed={}", git.join(f).display());
        }
        if let Ok(head) = std::fs::read_to_string(git.join("HEAD")) {
            if let Some(r) = head.trim().strip_prefix("ref: ") {
                println!("cargo:rerun-if-changed={}", git.join(r).display());
            }
        }
    }
}

/// The `.git` folder above `dir`, if it is one (not a worktree's `.git` file).
fn git_dir(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().map(|d| d.join(".git")).find(|g| g.is_dir())
}

/// Unix seconds as a UTC `YYYY-MM-DD` (Howard Hinnant's civil-from-days).
fn utc_date(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}
