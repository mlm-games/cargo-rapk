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
use crate::ndk::Ndk;
use std::{
    collections::BTreeSet,
    fs,
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

/// Every artifact `gavs` reaches, fetched and ready for the classpath.
///
/// `android_libs` names direct dependencies; this follows their POMs so a
/// library's own dependencies do not have to be listed by hand, and picks one
/// version per artifact the way Maven does when two paths disagree.
///
/// The result is ordered by coordinates, so a build gets the same classpath
/// twice.
pub fn resolve_libs(gavs: &[String]) -> Result<Vec<ResolvedLib>, NdkError> {
    let fail = |reason: String| NdkError::MavenLibFailed {
        gav: if gavs.is_empty() {
            "<none>".to_owned()
        } else {
            gavs.join(", ")
        },
        reason,
    };
    let roots = gavs
        .iter()
        .map(|gav| Coordinates::parse(gav).map_err(&fail))
        .collect::<Result<Vec<_>, _>>()?;
    crate::pom::resolve(&roots)?
        .iter()
        .map(|resolved| ensure_lib_as(&resolved.coordinates.gav(), resolved.extension.as_deref()))
        .collect()
}

/// A resolved Android library.
#[derive(Debug, Clone)]
pub struct ResolvedLib {
    pub coordinates: Coordinates,
    pub jars: Vec<PathBuf>,
    /// The AAR's `res/` tree, if it has one. This is the extraction root; the
    /// tree aapt2 compiles is its `res` child.
    pub resources: Option<PathBuf>,
    /// The library's own `package`, which its `R` class is generated under.
    pub package: Option<String>,
    /// `AndroidManifest.xml`, if the AAR carries one.
    pub manifest: Option<String>,
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

fn manifest_package(xml: &str) -> Option<String> {
    let open = xml.find("<manifest")?;
    let tag = &xml[open..];
    let end = tag.find('>')?;
    let attr = tag[..end].find("package=")? + "package=".len();
    let rest = &tag[attr..];
    let quote = rest.chars().next()?;
    if !matches!(quote, '"' | '\'') {
        return None;
    }
    rest[1..]
        .find(quote)
        .map(|end| rest[1..1 + end].to_string())
}

/// Element names an AAR manifest contributes to the merged manifest. Anything
/// but these two is dropped by the packaging pipeline, and a dropped provider or
/// activity only shows up as a crash on device.
pub(crate) fn manifest_contributions(xml: &str) -> Result<BTreeSet<String>, quick_xml::Error> {
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

/// Extracts an AAR's `res/` tree. Entry names are checked by the zip reader, so
/// a `../` path cannot escape `dest`.
fn extract_res(archive_path: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("not a valid archive: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("unreadable archive entry: {e}"))?;
        let name = entry.name().to_owned();
        let Some(relative) = name.strip_prefix("res/") else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        // `relative` is the path *under* `res/`; joining the archive's own
        // `res/...` path onto `dest` would nest a second `res` and leave the
        // tree aapt2 compiles empty. `enclosed_name` is still consulted first,
        // since it is what rejects a `../` entry escaping `dest`.
        if entry.enclosed_name().is_none() {
            continue;
        }
        let out_path = dest.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Generates one `R.java` per library against the shared resource table.
///
/// This has to run *after* the app's own `aapt2 link`, over the same compiled
/// overlays: a `--static-lib` link would emit placeholder ids (`0x0`) and
/// library code would then fail at runtime with "The key must be an
/// application-specific resource id".
pub fn generate_library_r(
    ndk: &Ndk,
    overlays: &[PathBuf],
    libraries: &[(String, String)],
    min_sdk_version: u32,
    target_sdk_version: u32,
    out_dir: &Path,
) -> Result<Vec<PathBuf>, NdkError> {
    if libraries.is_empty() {
        return Ok(Vec::new());
    }
    if overlays.is_empty() {
        return Err(NdkError::MavenLibFailed {
            gav: libraries.len().to_string(),
            reason: "library resources requested but no compiled overlays are available".to_owned(),
        });
    }
    let _ = out_dir;
    let android_jar = ndk.android_jar(target_sdk_version)?;

    let mut generated = Vec::new();
    for (index, (package, _manifest)) in libraries.iter().enumerate() {
        let dir = out_dir.join(index.to_string());
        std::fs::create_dir_all(&dir)?;

        // aapt2 validates the `package` attribute, and only writes below `--java`.
        let manifest_path = dir.join("AndroidManifest.xml");
        std::fs::write(
            &manifest_path,
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <manifest xmlns:android=\"http://schemas.android.com/apk/res/android\"\n\
                 \x20   package=\"{package}\" />\n"
            ),
        )?;

        // aapt2 requires an output path; the archive itself is irrelevant here,
        // only the generated R.java is read back.
        let out_apk = dir.join("overlay.apk");
        let mut cmd = ndk.build_tool("aapt2")?;
        cmd.arg("link")
            .arg("--java")
            .arg(&dir)
            .arg("-o")
            .arg(&out_apk)
            .arg("--manifest")
            .arg(&manifest_path)
            .arg("-I")
            .arg(&android_jar)
            .arg("--auto-add-overlay")
            .arg("--min-sdk-version")
            .arg(min_sdk_version.to_string());
        for overlay in overlays {
            cmd.arg("-R").arg(overlay);
        }
        if !cmd.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(cmd)));
        }

        let r_java = dir.join(package.replace('.', "/")).join("R.java");
        if !r_java.is_file() {
            return Err(NdkError::MavenLibFailed {
                gav: package.clone(),
                reason: format!("aapt2 generated no R.java at {}", r_java.display()),
            });
        }
        generated.push(r_java);
    }
    generated.sort();
    Ok(generated)
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

/// Reads `AndroidManifest.xml` out of an already-downloaded archive.
fn read_archive_manifest(archive: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(archive).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    (0..zip.len()).find_map(|i| {
        let mut entry = zip.by_index(i).ok()?;
        if entry.name() != "AndroidManifest.xml" {
            return None;
        }
        let mut xml = String::new();
        entry.read_to_string(&mut xml).ok()?;
        Some(xml)
    })
}

/// The archive `ensure_lib` downloaded, preferring the AAR since a jar of the
/// same name may sit beside it.
fn cached_archive(dir: &Path, coordinates: &Coordinates) -> Option<PathBuf> {
    let stem = format!("{}-{}", coordinates.artifact, coordinates.version);
    [".aar", ".jar"]
        .into_iter()
        .map(|ext| dir.join(format!("{stem}{ext}")))
        .find(|p| p.is_file())
}

/// Re-reads an already-extracted library from the cache, so a cached artifact
/// costs no network access and no re-extraction.
fn read_resolved(dir: &Path, coordinates: Coordinates) -> Option<ResolvedLib> {
    let mut jars: Vec<PathBuf> = std::fs::read_dir(dir.join("jars"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jar"))
        .collect();
    jars.sort();
    if jars.is_empty() {
        return None;
    }
    // The manifest is re-read from the archive rather than kept in a sidecar
    // file: an XML document spans many lines, so a `key=value` encoding of it
    // truncates at the first newline and silently yields a manifest with no
    // contributions in it.
    let library_manifest =
        cached_archive(dir, &coordinates).and_then(|a| read_archive_manifest(&a));
    let package = library_manifest
        .as_deref()
        .and_then(manifest_package)
        .filter(|p| !p.is_empty());
    let manifest = library_manifest.filter(|xml| {
        manifest_contributions(xml)
            .map(|c| !c.is_empty())
            .unwrap_or(true)
    });
    let resources = {
        let res = dir.join("res");
        res.is_dir().then_some(res)
    };
    Some(ResolvedLib {
        coordinates,
        jars,
        resources,
        package,
        manifest,
    })
}

/// Fetches (or reuses) one `android_libs` entry: its jars, `res/` tree and
/// manifest, all cached under the artifact directory.
///
/// This fetches one artifact. Use [`resolve_libs`] to walk the dependency graph
/// as well.
pub fn ensure_lib(gav: &str) -> Result<ResolvedLib, NdkError> {
    ensure_lib_as(gav, None)
}

/// As [`ensure_lib`], but fetching `extension` when the module's metadata named
/// one. A POM can only say "aar or jar", so `None` probes for an AAR first and
/// then a jar; Gradle Module Metadata says which, and taking it is what keeps a
/// stub jar published under a multiplatform module's own coordinates off the
/// classpath.
pub fn ensure_lib_as(gav: &str, extension: Option<&str>) -> Result<ResolvedLib, NdkError> {
    let coordinates = Coordinates::parse(gav).map_err(|reason| NdkError::MavenLibFailed {
        gav: gav.to_owned(),
        reason,
    })?;
    let dir = artifact_dir(&coordinates);
    let marker = marker_path(&coordinates);

    if !fetch_forced()
        && std::fs::read_to_string(&marker).is_ok_and(|m| m.trim() == coordinates.gav())
        && let Some(lib) = read_resolved(&dir, coordinates.clone())
    {
        return Ok(lib);
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

    let wanted = extension.unwrap_or("aar");
    let (archive, is_aar): (Option<PathBuf>, bool) = match fetch_artifact(
        &dir,
        &coordinates.group,
        &coordinates.artifact,
        &coordinates.version,
        wanted,
    ) {
        Ok(found) => (found, wanted == "aar"),
        Err(first_err) => {
            // A POM names no archive, so a jar is tried when no AAR answers.
            if wanted == "jar" {
                return Err(fail(first_err));
            }
            match fetch_artifact(
                &dir,
                &coordinates.group,
                &coordinates.artifact,
                &coordinates.version,
                "jar",
            ) {
                Ok(found) => (found, false),
                _ => return Err(fail(first_err)),
            }
        }
    };
    let archive = archive.ok_or_else(|| {
        fail(format!(
            "no `.aar` or `.jar` published for {gav} on {}",
            repo_base(&coordinates.group)
        ))
    })?;

    let library_manifest = read_archive_manifest(&archive);
    let package = library_manifest
        .as_deref()
        .and_then(manifest_package)
        .filter(|p| !p.is_empty());

    let jars_dir = dir.join("jars");
    let _ = std::fs::remove_dir_all(&jars_dir);
    let jars = extract_lib(&archive, &jars_dir, is_aar).map_err(&fail)?;

    let resources = if is_aar {
        let res_dir = dir.join("res");
        let _ = fs::remove_dir_all(&res_dir);
        extract_res(&archive, &res_dir).map_err(&fail)?;
        res_dir.is_dir().then_some(res_dir)
    } else {
        None
    };

    let manifest = library_manifest.filter(|xml| {
        manifest_contributions(xml)
            .map(|c| !c.is_empty())
            .unwrap_or(true)
    });

    // The marker alone identifies the cache entry; the manifest is re-read from
    // the archive, so nothing else needs persisting.
    let _ = std::fs::remove_file(dir.join(".cargo-rapk-lib-meta"));
    std::fs::write(&marker, coordinates.gav()).map_err(|e| fail(e.to_string()))?;
    Ok(ResolvedLib {
        coordinates,
        jars,
        resources,
        package,
        manifest,
    })
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
