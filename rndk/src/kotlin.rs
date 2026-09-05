//! Kotlin compiler lookup + fetch on demand.
//!
//! Only needed when `java_sources` contain `*.kt` (rlobkit-app-events does,
//! rlobkit-dialogs doesn't).
//!
//! Order: `CARGO_RAPK_KOTLINC`/`KOTLINC` path, `KOTLIN_HOME`, `which kotlinc`,
//! legacy `KOTLIN_COMPILER` path, then the cache
//! (`$XDG_CACHE_HOME/cargo-rapk/kotlin/kotlinc/bin/kotlinc`).
//! `KOTLIN_COMPILER` is never passed to the child: the kotlinc launcher
//! reuses that name for its Java main class.
//!
//! Missing compiler is fetched into the cache. `CARGO_RAPK_KOTLIN_VERSION`
//! (default `2.2.10`), `CARGO_RAPK_KOTLIN_SHA256`,
//! `CARGO_RAPK_FETCH_KOTLIN=never|force`, `CARGO_RAPK_NO_FETCH_KOTLIN=1`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::NdkError;

/// Default pin, but maybe set it to latest always (could be bad)?.
pub const DEFAULT_KOTLIN_VERSION: &str = "2.2.10";

const ENV_KOTLINC_PATH: &[&str] = &["CARGO_RAPK_KOTLINC", "KOTLINC"];
const ENV_KOTLIN_HOME: &str = "KOTLIN_HOME";
// Deprecated: collides with kotlinc launcher's own variable.
const ENV_LEGACY_COMPILER: &str = "KOTLIN_COMPILER";

pub fn kotlin_version() -> String {
    for key in ["CARGO_RAPK_KOTLIN_VERSION", "KOTLIN_VERSION"] {
        if let Ok(v) = std::env::var(key) {
            let v = v
                .trim()
                .trim_matches('"')
                .trim_start_matches('v')
                .to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    DEFAULT_KOTLIN_VERSION.to_string()
}

pub fn kotlin_cache_dir() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("cargo-rapk").join("kotlin")
}

pub fn expected_kotlinc_path() -> PathBuf {
    let p = kotlin_cache_dir()
        .join("kotlinc")
        .join("bin")
        .join("kotlinc");
    #[cfg(target_os = "windows")]
    let p = p.with_extension("bat");
    p
}

pub fn expected_stdlib_path() -> PathBuf {
    kotlin_cache_dir()
        .join("kotlinc")
        .join("lib")
        .join("kotlin-stdlib.jar")
}

fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

fn kotlinc_in_dir(dir: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        for name in ["kotlinc.bat", "kotlinc"] {
            let c = dir.join("bin").join(name);
            if is_executable_file(&c) {
                return Some(c);
            }
        }
        for name in ["kotlinc.bat"] {
            let c = dir.join(name);
            if is_executable_file(&c) {
                return Some(c);
            }
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        let c = dir.join("bin").join("kotlinc");
        if is_executable_file(&c) {
            return Some(c);
        }
        if dir.file_name().is_some_and(|n| n == "kotlinc") && is_executable_file(dir) {
            return Some(dir.to_path_buf());
        }
        None
    }
}

fn stdlib_next_to_kotlinc(kotlinc: &Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(kotlinc).unwrap_or_else(|_| kotlinc.to_path_buf());
    let bin_dir = resolved.parent()?;
    let home = bin_dir.parent()?;
    let jar = home.join("lib").join("kotlin-stdlib.jar");
    if jar.is_file() { Some(jar) } else { None }
}

/// Resolve an existing `kotlinc` without network. Returns `None` if absent.
pub fn resolve_kotlinc_path() -> Option<PathBuf> {
    for key in ENV_KOTLINC_PATH {
        if let Ok(v) = std::env::var(key) {
            let p = PathBuf::from(v.trim());
            if is_executable_file(&p) {
                return Some(p);
            }
        }
    }
    if let Ok(home) = std::env::var(ENV_KOTLIN_HOME) {
        let home = PathBuf::from(home.trim());
        if let Some(p) = kotlinc_in_dir(&home) {
            return Some(p);
        }
        let nested = home.join("kotlinc");
        if let Some(p) = kotlinc_in_dir(&nested) {
            return Some(p);
        }
    }
    if let Ok(p) = which::which("kotlinc") {
        return Some(p);
    }
    if let Ok(v) = std::env::var(ENV_LEGACY_COMPILER) {
        let p = PathBuf::from(v.trim());
        if is_executable_file(&p) {
            log::warn!(
                "env {ENV_LEGACY_COMPILER} is deprecated; use CARGO_RAPK_KOTLINC or KOTLIN_HOME"
            );
            return Some(p);
        }
    }
    let cached = expected_kotlinc_path();
    if is_executable_file(&cached) {
        return Some(cached);
    }
    None
}

/// Resolve an existing `kotlin-stdlib.jar` without network.
pub fn resolve_stdlib_path() -> Option<PathBuf> {
    if let Some(kotlinc) = resolve_kotlinc_path()
        && let Some(jar) = stdlib_next_to_kotlinc(&kotlinc)
    {
        return Some(jar);
    }
    let cached = expected_stdlib_path();
    if cached.is_file() {
        return Some(cached);
    }
    None
}

fn fetch_disabled() -> bool {
    if std::env::var("CARGO_RAPK_NO_FETCH_KOTLIN")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
    {
        return true;
    }
    matches!(
        std::env::var("CARGO_RAPK_FETCH_KOTLIN")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "never" | "never-fetch" | "no" | "0" | "false" | "off"
    )
}

fn fetch_forced() -> bool {
    matches!(
        std::env::var("CARGO_RAPK_FETCH_KOTLIN")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "force" | "always" | "1" | "true"
    )
}

fn install_instructions(version: &str) -> String {
    let cache = expected_kotlinc_path();
    format!(
        "kotlinc not found (java_sources contain *.kt). Install kotlinc {version}+, \
put it on PATH, or set CARGO_RAPK_KOTLINC / KOTLIN_HOME. Expected: {}. Manual: \
curl -fsSL -o /tmp/kotlin.zip https://github.com/JetBrains/kotlin/releases/download/v{version}/kotlin-compiler-{version}.zip \
&& unzip -q /tmp/kotlin.zip -d {}",
        cache.display(),
        kotlin_cache_dir().display(),
    )
}

/// Try fetch; on any network/tool failure fall back to manual instructions.
pub fn ensure_kotlinc() -> Result<PathBuf, NdkError> {
    if !fetch_forced()
        && let Some(p) = resolve_kotlinc_path()
    {
        return Ok(p);
    }
    let version = kotlin_version();
    if fetch_disabled() {
        return Err(NdkError::CmdNotFound(install_instructions(&version)));
    }
    log::info!("fetching Kotlin v{version} into cache");
    match fetch_kotlin_into_cache(&version) {
        Ok(()) => {}
        Err(e) => {
            return Err(NdkError::CmdNotFound(format!(
                "{e}. {}",
                install_instructions(&version)
            )));
        }
    }
    resolve_kotlinc_path().ok_or_else(|| NdkError::CmdNotFound(install_instructions(&version)))
}

/// Ensure `kotlin-stdlib.jar` exists (fetches the compiler if needed).
pub fn ensure_stdlib() -> Result<PathBuf, NdkError> {
    if let Some(p) = resolve_stdlib_path() {
        return Ok(p);
    }
    ensure_kotlinc()?;
    resolve_stdlib_path().ok_or_else(|| {
        NdkError::CmdNotFound(format!(
            "kotlin-stdlib.jar missing. Expected: {}",
            expected_stdlib_path().display()
        ))
    })
}

// Stripped from child env: kotlinc script uses this name for its main class.
pub fn kotlinc_command(path: &Path) -> Command {
    let mut cmd = Command::new(path);
    cmd.env_remove(ENV_LEGACY_COMPILER);
    cmd
}

fn kotlin_zip_url(version: &str) -> String {
    format!(
        "https://github.com/JetBrains/kotlin/releases/download/v{version}/kotlin-compiler-{version}.zip"
    )
}

fn download_with_curl_or_wget(url: &str, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if which::which("curl").is_ok() {
        let status = Command::new("curl")
            .arg("-fsSL")
            .arg("-o")
            .arg(dest)
            .arg(url)
            .status()
            .map_err(|e| format!("failed to run curl: {e}"))?;
        if status.success() {
            return Ok(());
        }
        return Err(format!("curl exited with {status} for {url}"));
    }
    if which::which("wget").is_ok() {
        let status = Command::new("wget")
            .arg("-qO")
            .arg(dest)
            .arg(url)
            .status()
            .map_err(|e| format!("failed to run wget: {e}"))?;
        if status.success() {
            return Ok(());
        }
        return Err(format!("wget exited with {status} for {url}"));
    }
    Err(
        "neither `curl` nor `wget` found on PATH; install one or pre-seed the cache manually"
            .into(),
    )
}

fn file_sha256_hex(path: &Path) -> Option<String> {
    for attempt in [
        vec!["sha256sum", &*path.to_string_lossy()],
        vec!["shasum", "-a", "256", &*path.to_string_lossy()],
    ] {
        let (bin, args) = (attempt[0], &attempt[1..]);
        if which::which(bin).is_err() {
            continue;
        }
        let out = Command::new(bin).args(args).output().ok()?;
        if !out.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let hex = stdout.split_whitespace().next().unwrap_or("").to_string();
        if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex.to_ascii_lowercase());
        }
    }
    None
}

fn expected_sha256(version: &str, zip_path: &Path) -> Option<String> {
    if let Ok(v) = std::env::var("CARGO_RAPK_KOTLIN_SHA256") {
        let v = v.trim().to_ascii_lowercase();
        if v.len() == 64 {
            return Some(v);
        }
        log::warn!("ignoring malformed CARGO_RAPK_KOTLIN_SHA256 (want 64 hex chars)");
    }
    let sidecar = format!("{}.sha256", kotlin_zip_url(version));
    let tmp = zip_path.with_extension("zip.sha256.dl");
    if download_with_curl_or_wget(&sidecar, &tmp).is_ok() {
        let text = std::fs::read_to_string(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);
        let hex = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex);
        }
        log::warn!(
            "could not parse {}.sha256 sidecar; skipping verification",
            version
        );
    }
    None
}

fn unzip_archive(zip_path: &Path, dest_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    if which::which("unzip").is_ok() {
        let status = Command::new("unzip")
            .arg("-q")
            .arg(zip_path)
            .arg("-d")
            .arg(dest_dir)
            .status()
            .map_err(|e| format!("failed to run unzip: {e}"))?;
        if status.success() {
            return Ok(());
        }
        log::warn!("system `unzip` failed ({status}); falling back to embedded extractor");
    }
    extract_with_zip_crate(zip_path, dest_dir)
}

fn extract_with_zip_crate(zip_path: &Path, dest_dir: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("bad kotlin zip: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let Some(out_path) = entry.enclosed_name().map(|p| dest_dir.join(p)) else {
            continue;
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = entry.unix_mode() {
                    let _ =
                        std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
                }
            }
        }
    }
    Ok(())
}

fn fetch_kotlin_into_cache(version: &str) -> Result<(), NdkError> {
    let cache_dir = kotlin_cache_dir();
    let kotlinc_path = expected_kotlinc_path();
    let stdlib_path = expected_stdlib_path();
    if kotlinc_path.is_file() && stdlib_path.is_file() && !fetch_forced() {
        return Ok(());
    }
    let url = kotlin_zip_url(version);
    let tmp_zip = cache_dir.join(format!("kotlin-compiler-{version}.zip.dl"));
    log::info!("downloading {url}");
    download_with_curl_or_wget(&url, &tmp_zip).map_err(|reason| NdkError::KotlinFetchFailed {
        version: version.to_string(),
        reason,
    })?;

    if let Some(expected) = expected_sha256(version, &tmp_zip) {
        match file_sha256_hex(&tmp_zip) {
            Some(actual) if actual == expected => log::info!("kotlin zip sha256 OK"),
            Some(actual) => {
                let _ = std::fs::remove_file(&tmp_zip);
                return Err(NdkError::KotlinChecksumMismatch { expected, actual });
            }
            None => log::warn!("no sha256 tool found; skipping checksum verification"),
        }
    } else {
        log::warn!("no usable .sha256 sidecar; skipping checksum verification");
    }

    let stage = cache_dir.join(format!(".stage-{version}"));
    let _ = std::fs::remove_dir_all(&stage);
    unzip_archive(&tmp_zip, &stage).map_err(|reason| NdkError::KotlinFetchFailed {
        version: version.to_string(),
        reason,
    })?;
    let staged_inner = stage.join("kotlinc");
    if !staged_inner.is_dir() {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(NdkError::KotlinFetchFailed {
            version: version.to_string(),
            reason: "zip did not contain top-level kotlinc/ dir".into(),
        });
    }
    let target_inner = cache_dir.join("kotlinc");
    let _ = std::fs::remove_dir_all(&target_inner);
    std::fs::rename(&staged_inner, &target_inner).map_err(|e| NdkError::KotlinFetchFailed {
        version: version.to_string(),
        reason: format!("failed to move kotlinc into cache: {e}"),
    })?;
    let _ = std::fs::remove_dir_all(&stage);
    let _ = std::fs::remove_file(&tmp_zip);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = target_inner.join("bin").join("kotlinc");
        if bin.is_file() {
            let _ = std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755));
        }
    }

    if !expected_kotlinc_path().is_file() {
        return Err(NdkError::KotlinFetchFailed {
            version: version.to_string(),
            reason: "unzip succeeded but kotlinc/bin/kotlinc missing".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_defaults_and_trims() {
        assert!(!kotlin_version().is_empty());
    }

    #[test]
    fn expected_paths_have_jetbrains_layout() {
        assert!(expected_kotlinc_path().ends_with("kotlinc/bin/kotlinc"));
        assert!(expected_stdlib_path().ends_with("lib/kotlin-stdlib.jar"));
    }

    #[test]
    fn legacy_class_name_is_not_a_path() {
        let class = PathBuf::from("org.jetbrains.kotlin.cli.jvm.K2JVMCompiler");
        assert!(!class.exists());
    }
}
