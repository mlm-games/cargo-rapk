//! Maven repository access.
//!
//! Shared download/verify helpers (the Kotlin compiler fetch and the
//! `android_libs` artifact fetch both use them) plus the artifact resolver
//! behind `android_libs`.
//!
//! Google's Maven (`androidx.*`, `com.android.*`, the Android games SDK) and
//! Maven Central both publish `.sha1` sidecars, so every download is verified
//! fail-closed. A resolved artifact is extracted into the cache and reused
//! without touching the network again.

use crate::error::NdkError;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

pub const CENTRAL_BASE: &str = "https://repo.maven.apache.org/maven2";
pub const GOOGLE_BASE: &str = "https://dl.google.com/dl/android/maven2";

pub fn repo_base(group: &str) -> &'static str {
    if group.starts_with("androidx.")
        || group.starts_with("com.android.")
        || group.starts_with("com.google.android")
    {
        GOOGLE_BASE
    } else {
        CENTRAL_BASE
    }
}

pub fn artifact_url(group: &str, artifact: &str, version: &str, ext: &str) -> String {
    format!(
        "{}/{}/{}/{}/{}-{}.{}",
        repo_base(group),
        group.replace('.', "/"),
        artifact,
        version,
        artifact,
        version,
        ext
    )
}

pub fn download_with_curl_or_wget(url: &str, dest: &Path) -> Result<(), String> {
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

pub fn download_text(url: &str) -> Result<String, String> {
    let tmp = std::env::temp_dir().join(format!("cargo-rapk-maven-{}", std::process::id()));
    download_with_curl_or_wget(url, &tmp)?;
    let text = std::fs::read_to_string(&tmp).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&tmp);
    Ok(text)
}

fn file_digest_hex(path: &Path, tool: &str, shasum_bits: &str, hex_len: usize) -> Option<String> {
    for attempt in [
        vec![tool, &*path.to_string_lossy()],
        vec!["shasum", "-a", shasum_bits, &*path.to_string_lossy()],
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
        if hex.len() == hex_len && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex.to_ascii_lowercase());
        }
    }
    None
}

pub fn file_sha256_hex(path: &Path) -> Option<String> {
    file_digest_hex(path, "sha256sum", "256", 64)
}

pub fn verify_sha1(path: &Path, expected: &str) -> bool {
    let actual = file_digest_hex(path, "sha1sum", "1", 40);
    actual.is_some_and(|a| a == expected.trim().to_ascii_lowercase())
}

pub fn url_definitely_missing(url: &str) -> Option<bool> {
    if which::which("curl").is_err() {
        return None;
    }
    match Command::new("curl")
        .arg("-fsSI")
        .arg("-o")
        .arg("/dev/null")
        .arg(url)
        .output()
    {
        Ok(out) if out.status.success() => Some(false),
        Ok(out) if out.status.code() == Some(22) => Some(true),
        _ => None,
    }
}

/// Download one Maven artifact + fail-closed `.sha1` check. Reuses verified files.
pub fn fetch_artifact(
    dir: &Path,
    group: &str,
    artifact: &str,
    version: &str,
    ext: &str,
) -> Result<Option<PathBuf>, String> {
    fetch_artifact_inner(dir, group, artifact, version, ext, false)
}

pub fn fetch_artifact_inner(
    dir: &Path,
    group: &str,
    artifact: &str,
    version: &str,
    ext: &str,
    allow_unpublished: bool,
) -> Result<Option<PathBuf>, String> {
    let jar_url = artifact_url(group, artifact, version, ext);
    let dest = dir.join(format!("{artifact}-{version}.{ext}"));
    // Stale POMs can reference artifacts never published for this version
    if allow_unpublished && url_definitely_missing(&jar_url) == Some(true) {
        log::warn!("skipping unpublished {artifact}-{version}.{ext}");
        return Ok(None);
    }
    let sha_url = artifact_url(group, artifact, version, &format!("{ext}.sha1"));
    let want = download_text(&sha_url)
        .ok()
        .and_then(|t| t.split_whitespace().next().map(str::to_string))
        .ok_or_else(|| format!("no usable .sha1 sidecar for {artifact}-{version}.{ext}"))?;
    if dest.is_file() && verify_sha1(&dest, &want) {
        return Ok(Some(dest));
    }
    let _ = std::fs::remove_file(&dest);
    download_with_curl_or_wget(&artifact_url(group, artifact, version, ext), &dest)?;
    if !verify_sha1(&dest, &want) {
        let _ = std::fs::remove_file(&dest);
        return Err(format!("sha1 mismatch for {artifact}-{version}.{ext}"));
    }
    Ok(Some(dest))
}

/// A `group:artifact:version` triple as written in `android_libs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coordinates {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

impl Coordinates {
    pub fn parse(gav: &str) -> Result<Self, String> {
        let mut parts = gav.split(':');
        let (Some(group), Some(artifact), Some(version), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(format!(
                "`{gav}` is not `group:artifact:version` (e.g. \
                 `androidx.games:games-activity:4.4.0`)"
            ));
        };
        if group.is_empty() || artifact.is_empty() || version.is_empty() {
            return Err(format!("`{gav}` has an empty group, artifact or version"));
        }
        Ok(Self {
            group: group.to_owned(),
            artifact: artifact.to_owned(),
            version: version.to_owned(),
        })
    }

    pub fn gav(&self) -> String {
        format!("{}:{}:{}", self.group, self.artifact, self.version)
    }
}

/// A resolved Android library: the jars that go on the compile classpath and
/// into the dex.
#[derive(Debug, Clone)]
pub struct ResolvedLib {
    pub coordinates: Coordinates,
    pub jars: Vec<PathBuf>,
}

pub fn cache_dir() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("cargo-rapk").join("maven")
}

pub fn artifact_dir(coordinates: &Coordinates) -> PathBuf {
    cache_dir()
        .join(coordinates.group.replace('.', "/"))
        .join(&coordinates.artifact)
        .join(&coordinates.version)
}

fn marker_path(coordinates: &Coordinates) -> PathBuf {
    artifact_dir(coordinates).join(".cargo-rapk-lib")
}

pub fn fetch_disabled() -> bool {
    for key in ["CARGO_RAPK_NO_FETCH_MAVEN", "CARGO_RAPK_FETCH_MAVEN"] {
        match std::env::var(key) {
            Ok(v) if v.trim().eq_ignore_ascii_case("never") || v.trim() == "1" => return true,
            _ => {}
        }
    }
    false
}

fn fetch_forced() -> bool {
    std::env::var("CARGO_RAPK_FETCH_MAVEN").is_ok_and(|v| v.trim().eq_ignore_ascii_case("force"))
}

/// Element names an AAR manifest contributes to the merged manifest. Anything
/// but these two is dropped by the packaging pipeline, and a dropped provider or
/// activity only shows up as a crash on device.
fn manifest_contributions(xml: &str) -> Result<BTreeSet<String>, quick_xml::Error> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut found = BTreeSet::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name != "manifest" && name != "uses-sdk" {
                    found.insert(name);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(found)
}

fn res_entries(archive: &mut zip::ZipArchive<std::fs::File>) -> Vec<String> {
    let mut names = Vec::new();
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i)
            && !entry.is_dir()
            && entry.name().starts_with("res/")
        {
            names.push(entry.name().to_owned());
        }
    }
    names
}

fn extract_lib(archive_path: &Path, dest: &Path, is_aar: bool) -> Result<Vec<PathBuf>, String> {
    if !is_aar {
        let file_name = archive_path
            .file_name()
            .ok_or_else(|| format!("{} has no file name", archive_path.display()))?;
        let out_path = dest.join(file_name);
        std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
        std::fs::copy(archive_path, &out_path).map_err(|e| e.to_string())?;
        return Ok(vec![out_path]);
    }
    let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("not a valid archive: {e}"))?;
    let mut jars = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("unreadable archive entry: {e}"))?;
        let name = entry.name().to_owned();
        if entry.is_dir() {
            continue;
        }
        if name != "classes.jar" && !(name.starts_with("libs/") && name.ends_with(".jar")) {
            continue;
        }
        let file_name = name
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| format!("archive entry `{name}` has no file name"))?;
        let out_path = dest.join(file_name);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        jars.push(out_path);
    }
    jars.sort();
    if jars.is_empty() {
        return Err("archive holds no `classes.jar` and no `libs/*.jar`".to_string());
    }
    Ok(jars)
}

/// Fetch (or reuse) one `android_libs` entry and return its jars.
///
/// Only artifacts that are pure class containers are accepted: an AAR carrying
/// `res/` or manifest entries needs a resource/manifest merger, and silently
/// ignoring them would produce an app that fails at runtime instead of at build
/// time. Use Gradle for those.
pub fn ensure_lib(gav: &str) -> Result<ResolvedLib, NdkError> {
    let coordinates = Coordinates::parse(gav).map_err(|reason| NdkError::MavenLibFailed {
        gav: gav.to_owned(),
        reason,
    })?;
    let dir = artifact_dir(&coordinates);
    let marker = marker_path(&coordinates);

    if !fetch_forced()
        && std::fs::read_to_string(&marker).is_ok_and(|m| m.trim() == coordinates.gav())
    {
        let mut jars: Vec<PathBuf> = std::fs::read_dir(dir.join("jars"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jar"))
            .collect();
        jars.sort();
        if !jars.is_empty() {
            return Ok(ResolvedLib { coordinates, jars });
        }
    }

    let fail = |reason: String| NdkError::MavenLibFailed {
        gav: gav.to_owned(),
        reason,
    };

    if fetch_disabled() {
        return Err(fail(format!(
            "not in {} and fetching is disabled; set CARGO_RAPK_FETCH_MAVEN, or unset \
             CARGO_RAPK_NO_FETCH_MAVEN",
            dir.display()
        )));
    }

    let (archive, is_aar): (Option<PathBuf>, bool) = match fetch_artifact(
        &dir,
        &coordinates.group,
        &coordinates.artifact,
        &coordinates.version,
        "aar",
    ) {
        Ok(found) => (found, true),
        Err(aar_err) => {
            // Not every Maven artifact ships an AAR; pure-JVM libraries are jars.
            match fetch_artifact(
                &dir,
                &coordinates.group,
                &coordinates.artifact,
                &coordinates.version,
                "jar",
            ) {
                Ok(found) => (found, false),
                _ => return Err(fail(aar_err)),
            }
        }
    };
    let archive = archive.ok_or_else(|| {
        fail(format!(
            "no `.aar` or `.jar` published for {gav} on {}",
            repo_base(&coordinates.group)
        ))
    })?;

    let file = std::fs::File::open(&archive).map_err(|e| fail(e.to_string()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| fail(e.to_string()))?;
    if is_aar {
        let res = res_entries(&mut zip);
        if !res.is_empty() {
            return Err(fail(format!(
                "carries {} resource file(s) (e.g. `{}`); library resource merging is not \
                 supported, use a Gradle build",
                res.len(),
                res[0]
            )));
        }
        let mut library_manifest = None;
        for i in 0..zip.len() {
            if let Ok(mut entry) = zip.by_index(i)
                && entry.name() == "AndroidManifest.xml"
            {
                use std::io::Read;
                let mut xml = String::new();
                entry
                    .read_to_string(&mut xml)
                    .map_err(|e| fail(format!("unreadable AndroidManifest.xml: {e}")))?;
                library_manifest = Some(xml);
                break;
            }
        }
        if let Some(xml) = library_manifest {
            let contributions = manifest_contributions(&xml).map_err(|e| fail(e.to_string()))?;
            if !contributions.is_empty() {
                return Err(fail(format!(
                    "declares <{}>; library manifest merging is not supported, use a Gradle build",
                    contributions.into_iter().collect::<Vec<_>>().join(">, <")
                )));
            }
        }
    }
    drop(zip);

    let jars_dir = dir.join("jars");
    let _ = std::fs::remove_dir_all(&jars_dir);
    let jars = extract_lib(&archive, &jars_dir, is_aar).map_err(&fail)?;
    std::fs::write(&marker, coordinates.gav()).map_err(|e| fail(e.to_string()))?;
    Ok(ResolvedLib { coordinates, jars })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_is_chosen_by_group() {
        assert_eq!(
            artifact_url("androidx.games", "games-activity", "4.4.0", "aar"),
            "https://dl.google.com/dl/android/maven2/androidx/games/games-activity/4.4.0/games-activity-4.4.0.aar"
        );
        assert_eq!(
            artifact_url("org.jetbrains.kotlin", "kotlin-stdlib", "2.2.10", "jar"),
            "https://repo.maven.apache.org/maven2/org/jetbrains/kotlin/kotlin-stdlib/2.2.10/kotlin-stdlib-2.2.10.jar"
        );
    }

    #[test]
    fn parses_coordinates() {
        let parsed = Coordinates::parse("androidx.games:games-activity:4.4.0").unwrap();
        assert_eq!(parsed.group, "androidx.games");
        assert_eq!(parsed.artifact, "games-activity");
        assert_eq!(parsed.version, "4.4.0");
        assert!(Coordinates::parse("androidx.games:games-activity").is_err());
        assert!(Coordinates::parse("androidx.games:games-activity:4.4.0:extra").is_err());
        assert!(Coordinates::parse("androidx.games::4.4.0").is_err());
    }

    #[test]
    fn only_manifest_and_uses_sdk_count_as_no_contribution() {
        let bare = r#"<?xml version="1.0" encoding="utf-8"?>
            <manifest xmlns:android="http://schemas.android.com/apk/res/android"
                package="com.google.androidgamesdk.gameactivity" >
                <uses-sdk android:minSdkVersion="21" />
            </manifest>"#;
        assert!(manifest_contributions(bare).unwrap().is_empty());

        let contributing = r#"<?xml version="1.0" encoding="utf-8"?>
            <manifest xmlns:android="http://schemas.android.com/apk/res/android"
                xmlns:tools="http://schemas.android.com/tools" package="androidx.startup" >
                <uses-sdk android:minSdkVersion="21" />
                <application>
                    <provider android:name="androidx.startup.InitializationProvider"
                        android:authorities="${applicationId}.androidx-startup"
                        tools:node="merge" />
                </application>
            </manifest>"#;
        let found = manifest_contributions(contributing).unwrap();
        assert!(found.contains("application"));
        assert!(found.contains("provider"));
    }
}
