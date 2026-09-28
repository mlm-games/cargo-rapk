//! Maven repository access.
//!
//! Shared download/verify helpers (the Kotlin compiler fetch and the
//! `android_libs` artifact fetch both use them) plus the artifact resolver
//! behind `android_libs`.
//!
//! Every download is verified against the repository's `.sha1` sidecar when it
//! publishes one, and a mismatch fails. A resolved artifact is extracted into
//! the cache and reused without touching the network again.

use crate::error::NdkError;
use crate::ndk::Ndk;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub const CENTRAL_BASE: &str = "https://repo.maven.apache.org/maven2";
pub const GOOGLE_BASE: &str = "https://dl.google.com/dl/android/maven2";

/// One place to fetch artifacts from.
/// Only `url` is required. Every other field is optional, because
/// `AndroidMetadata` is deserialized out of the whole `Cargo.toml` in one pass
/// and a missing optional field would otherwise take the entire manifest with
/// it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Repository {
    /// The root the Maven layout is appended to, without a trailing slash.
    pub url: String,
    /// HTTP basic credentials, if the repository wants any.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// A bearer token, sent as `Authorization: Bearer`.
    #[serde(default)]
    pub token: Option<String>,
    /// Group prefixes this repository is searched for. Empty means every group.
    #[serde(default)]
    pub include_groups: Vec<String>,
    /// Group prefixes this repository is never searched for.
    #[serde(default)]
    pub exclude_groups: Vec<String>,
}

/// Replaces every `${NAME}` with that variable's value, leaving an unset one
/// as the literal text so the failure names what is missing.
fn interpolate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => {
                        out.push_str("${");
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            // No closing brace: `out` already holds the text before the `${`,
            // so only what follows it is left to append.
            None => return format!("{out}{}", &rest[start..]),
        }
    }
    out.push_str(rest);
    out
}

impl Repository {
    fn accepts(&self, group: &str) -> bool {
        let included = self.include_groups.is_empty()
            || self
                .include_groups
                .iter()
                .any(|p| group.starts_with(p.as_str()));
        included
            && !self
                .exclude_groups
                .iter()
                .any(|p| group.starts_with(p.as_str()))
    }

    /// This repository with `${NAME}` in its fields replaced from the
    /// environment.
    pub fn resolved(&self) -> Self {
        let mut out = self.clone();
        out.url = interpolate(&out.url);
        out.username = out.username.as_deref().map(interpolate);
        out.password = out.password.as_deref().map(interpolate);
        out.token = out.token.as_deref().map(interpolate);
        out
    }

    /// This repository's credentials, as a `curl` configuration.
    ///
    /// Every argument a running process was given is readable by any other
    /// process on the machine through `/proc/<pid>/cmdline`, so a token passed
    /// as `-H "Authorization: Bearer ..."` or a password as `-u user:pass` is on
    /// that list for as long as the download runs. `curl --config -` reads the
    /// same settings from standard input, which no other process can read.
    fn curl_config(&self) -> Option<String> {
        let mut config = String::new();
        if let Some(token) = &self.token {
            config.push_str(&format!(
                "header = {}\n",
                quote_curl_config(&format!("Authorization: Bearer {token}"))
            ));
        }
        if let Some(username) = &self.username {
            config.push_str(&format!(
                "user = {}\n",
                quote_curl_config(&format!(
                    "{username}:{}",
                    self.password.as_deref().unwrap_or_default()
                ))
            ));
        }
        (!config.is_empty()).then_some(config)
    }
}

/// A `curl` configuration value. The parser reads a double-quoted string in
/// which a backslash escapes the next character, so a value containing a quote
/// or a backslash has to carry one.
fn quote_curl_config(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        if matches!(character, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

/// Runs `curl` with its credentials on standard input, so no secret reaches the
/// process table. The body still goes wherever `args` says.
fn curl_authenticated(args: &[&OsStr], config: &str) -> Result<std::process::Output, String> {
    use std::io::Write;
    let mut child = Command::new("curl")
        .arg("-fsSL")
        .arg("--config")
        .arg("-")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run curl: {e}"))?;
    // The configuration is read before the request starts, so this never waits
    // on the child for room in the pipe.
    child
        .stdin
        .take()
        .ok_or_else(|| "curl was started without a pipe for its configuration".to_owned())?
        .write_all(config.as_bytes())
        .map_err(|e| format!("failed to send curl its configuration: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("failed to run curl: {e}"))?;
    if out.status.success() {
        return Ok(out);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(format!(
        "curl exited with {}: {}",
        out.status,
        stderr.trim()
    ))
}

static REPOSITORIES: std::sync::OnceLock<Vec<Repository>> = std::sync::OnceLock::new();

/// The repositories to search, in order.
///
/// With nothing configured this is Google Maven then Maven Central, the order
/// an Android `settings.gradle` conventionally lists. Searching both rather
/// than choosing one from the group prefix is what a repository means: a
/// prefix is a guess, and it is wrong for `com.google.firebase`, which is on
/// Google Maven and not on Central.
pub fn repositories() -> &'static [Repository] {
    REPOSITORIES.get_or_init(|| {
        let configured = std::env::var("CARGO_RAPK_MAVEN_REPOSITORIES")
            .ok()
            .and_then(|raw| parse_repository_list(&raw));
        match configured {
            Some(list) if !list.is_empty() => list,
            _ => {
                let mut list = vec![Repository {
                    url: GOOGLE_BASE.to_owned(),
                    username: None,
                    password: None,
                    token: None,
                    include_groups: Vec::new(),
                    exclude_groups: Vec::new(),
                }];
                if let Ok(mirror) = std::env::var("CARGO_RAPK_MAVEN_CENTRAL") {
                    list.push(Repository {
                        url: interpolate(&mirror),
                        ..list[0].clone()
                    });
                } else {
                    list.push(Repository {
                        url: CENTRAL_BASE.to_owned(),
                        ..list[0].clone()
                    });
                }
                if let Ok(mirror) = std::env::var("CARGO_RAPK_MAVEN_GOOGLE") {
                    list[0].url = interpolate(&mirror);
                }
                list
            }
        }
        .iter()
        .map(|r| r.resolved())
        .collect()
    })
}

/// Reads a repository list from an environment variable, as either a bare array
/// or the `[[repository]]` form `Cargo.toml` uses.
fn parse_repository_list(raw: &str) -> Option<Vec<Repository>> {
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        repository: Vec<Repository>,
    }
    if let Ok(list) = toml::from_str::<Vec<Repository>>(raw) {
        return Some(list);
    }
    toml::from_str::<Wrapper>(raw)
        .ok()
        .map(|w| w.repository)
        .filter(|list| !list.is_empty())
}

/// Replaces the repository list. Intended for an embedder that already has one.
pub fn set_repositories(list: Vec<Repository>) {
    let _ = REPOSITORIES.set(list);
}

/// The repositories that serve `group`, in the order they are searched.
///
/// Empty when none does, which is a reportable condition: falling back to every
/// configured repository would send a group to one that was configured never to
/// serve it, credentials included.
pub fn repo_bases(group: &str) -> Vec<&'static Repository> {
    repositories().iter().filter(|r| r.accepts(group)).collect()
}

/// Why no repository serves `group`, for when none does.
pub fn unserved(group: &str) -> String {
    let configured: Vec<String> = repositories()
        .iter()
        .map(|r| {
            format!(
                "{} (include_groups={:?}, exclude_groups={:?})",
                r.url, r.include_groups, r.exclude_groups
            )
        })
        .collect();
    format!(
        "no configured repository serves `{group}`; configured: {}",
        configured.join(", ")
    )
}

/// The first repository that serves `group`, for a message about where a lookup
/// would have gone.
pub fn repo_base(group: &str) -> &'static str {
    repo_bases(group)
        .first()
        .map(|r| r.url.as_str())
        .unwrap_or(CENTRAL_BASE)
}

pub fn artifact_url(
    repository: &str,
    group: &str,
    artifact: &str,
    version: &str,
    ext: &str,
) -> String {
    artifact_url_classified(repository, group, artifact, version, ext, None)
}

/// As [`artifact_url`], for an archive published under a classifier. Maven
/// names those `{artifact}-{version}-{classifier}.{ext}`.
pub fn artifact_url_classified(
    repository: &str,
    group: &str,
    artifact: &str,
    version: &str,
    ext: &str,
    classifier: Option<&str>,
) -> String {
    let name = match classifier {
        Some(classifier) => format!("{artifact}-{version}-{classifier}"),
        None => format!("{artifact}-{version}"),
    };
    format!(
        "{}/{}/{}/{}/{name}.{}",
        repository.trim_end_matches('/'),
        group.replace('.', "/"),
        artifact,
        version,
        ext
    )
}

/// As [`download_with_curl_or_wget`], sending this repository's credentials.
pub fn download_authenticated(
    url: &str,
    dest: &Path,
    repository: &Repository,
) -> Result<(), String> {
    let Some(config) = repository.curl_config() else {
        return download_with_curl_or_wget(url, dest);
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    curl_authenticated(
        &[OsStr::new("-o"), dest.as_os_str(), OsStr::new(url)],
        &config,
    )?;
    Ok(())
}

/// Reads a URL to a string, sending this repository's credentials.
pub fn download_text_authenticated(url: &str, repository: &Repository) -> Result<String, String> {
    let Some(config) = repository.curl_config() else {
        return download_text(url);
    };
    let out =
        curl_authenticated(&[OsStr::new(url)], &config).map_err(|e| format!("{e} for {url}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

/// Download one Maven artifact, verifying it against a `.sha1` sidecar when
/// the repository publishes one. Reuses files that already verify.
pub fn fetch_artifact(
    dir: &Path,
    group: &str,
    artifact: &str,
    version: &str,
    ext: &str,
) -> Result<Option<PathBuf>, String> {
    fetch_artifact_inner(dir, group, artifact, version, ext, None, false)
}

pub fn fetch_artifact_inner(
    dir: &Path,
    group: &str,
    artifact: &str,
    version: &str,
    ext: &str,
    classifier: Option<&str>,
    allow_unpublished: bool,
) -> Result<Option<PathBuf>, String> {
    let stem = match classifier {
        Some(classifier) => format!("{artifact}-{version}-{classifier}"),
        None => format!("{artifact}-{version}"),
    };
    let dest = dir.join(format!("{stem}.{ext}"));
    // A stale POM can reference an artifact no repository ever published, which
    // is worth recognising before spending a download per repository on it.
    if allow_unpublished {
        let everywhere = repo_bases(group).iter().all(|r| {
            url_definitely_missing(&artifact_url_classified(
                &r.url, group, artifact, version, ext, classifier,
            )) == Some(true)
        });
        if everywhere {
            log::warn!("skipping unpublished {artifact}-{version}.{ext}");
            return Ok(None);
        }
    }

    // Repositories are searched in order and the first that serves the file
    // wins, which is what makes an artifact reachable regardless of which
    // mirror happens to carry it. A sidecar that is present and disagrees is a
    // hard failure; a repository that publishes no sidecar is reported and the
    // download goes ahead, as Maven and Gradle both do.
    let mut tried: Vec<String> = Vec::new();
    for repository in repo_bases(group) {
        let url =
            artifact_url_classified(&repository.url, group, artifact, version, ext, classifier);
        let sha_url = format!("{url}.sha1");
        let want = download_text_authenticated(&sha_url, repository)
            .ok()
            .and_then(|t| {
                t.split_whitespace()
                    .next()
                    .map(str::to_string)
                    .filter(|digest| digest.len() == 40)
            });
        // A repository that publishes no sidecar cannot contradict one, so a
        // file already fetched from it is reused rather than pulled again.
        match &want {
            Some(want) if dest.is_file() && verify_sha1(&dest, want) => return Ok(Some(dest)),
            None if dest.is_file() => return Ok(Some(dest)),
            _ => {}
        }
        let _ = std::fs::remove_file(&dest);
        match download_authenticated(&url, &dest, repository) {
            Ok(()) => {}
            Err(reason) => {
                tried.push(reason);
                continue;
            }
        }
        return match want {
            Some(want) if !verify_sha1(&dest, &want) => {
                let _ = std::fs::remove_file(&dest);
                Err(format!(
                    "sha1 mismatch for {artifact}-{version}.{ext} from {url}"
                ))
            }
            None => {
                log::warn!(
                    "{artifact}-{version}.{ext} has no .sha1 sidecar at {sha_url}; \
                     the download is unverified"
                );
                Ok(Some(dest))
            }
            _ => Ok(Some(dest)),
        };
    }
    if tried.is_empty() {
        return Err(unserved(group));
    }
    Err(format!(
        "no repository publishes {artifact}-{version}.{ext}; tried:\n  {}",
        tried.join("\n  ")
    ))
}

/// A `group:artifact:version` triple as written in `android_libs`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Coordinates {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

/// How an `android_libs` entry says its module should be treated.
///
/// Gradle takes this from the *declaration* — `platform("...")` versus a plain
/// one — and never from the module itself, so a `pom`-packaged aggregator
/// contributes no graph-wide version constraints when it is named normally.
/// `<packaging>` cannot answer this, so the declaration has to say which it is,
/// and an unmarked entry is guessed from the packaging as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    /// Guess: a `pom`-packaged module carrying a `<dependencyManagement>` is a
    /// platform, and anything else is a library.
    #[default]
    Auto,
    /// A platform. Its versions are constraints on the whole graph and it ships
    /// no artifact, whatever its packaging.
    Platform,
    /// A library, even when its packaging would guess otherwise. Its
    /// `<dependencyManagement>` is not applied to the graph and its
    /// `<dependencies>` are resolved, which is what Gradle's library variant
    /// does.
    Library,
}

impl Role {
    fn parse(suffix: &str) -> Result<Self, String> {
        match suffix {
            "" => Ok(Self::Auto),
            "platform" => Ok(Self::Platform),
            "library" => Ok(Self::Library),
            other => Err(format!(
                "`{other}` is not a role; an `android_libs` entry may be suffixed \
                 `!platform` or `!library`"
            )),
        }
    }
}

/// One `android_libs` entry: what to fetch, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub coordinates: Coordinates,
    pub role: Role,
}

impl Coordinates {
    pub fn parse(gav: &str) -> Result<Self, String> {
        Ok(parse_entry(gav)?.coordinates)
    }
}

/// An `android_libs` entry, as written: `group:artifact:version` with an
/// optional `!platform` or `!library` suffix.
pub fn parse_entry(gav: &str) -> Result<Root, String> {
    let gav = gav.trim();
    let (gav, suffix) = match gav.split_once('!') {
        Some((gav, suffix)) => (gav, suffix),
        None => (gav, ""),
    };
    let role = Role::parse(suffix.trim())?;
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
    Ok(Root {
        coordinates: Coordinates {
            group: group.to_owned(),
            artifact: artifact.to_owned(),
            version: version.to_owned(),
        },
        role,
    })
}

impl Coordinates {
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
        .map(|gav| parse_entry(gav).map_err(&fail))
        .collect::<Result<Vec<_>, _>>()?;
    crate::pom::resolve(&roots)?
        .iter()
        .filter_map(|resolved| {
            ensure_lib_inner(
                &resolved.coordinates.gav(),
                resolved.extension.as_deref(),
                resolved.classifier.as_deref(),
                resolved.optional,
            )
            .transpose()
        })
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
    /// The library's own `uses-sdk` `minSdkVersion`, which the app has to meet
    /// or the library's code runs on an API level it was never built for.
    pub min_sdk_version: Option<u32>,
    /// The classifier the archive was published under, when one was asked for.
    pub classifier: Option<String>,
}

/// The last two path components of a cache path: `<artifact>/<version>/<file>`.
fn file_label(archive: &Path) -> String {
    let mut parts: Vec<String> = archive
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.reverse();
    parts.join("/")
}

/// A class that more than one artifact on the classpath defines, with every
/// artifact that defines it.
///
/// `d8` reports this itself, but as `Type … is defined multiple times` followed
/// by the entire classpath — thousands of paths, with the pair that actually
/// conflicts buried in it and nothing suggesting what to do. Naming the
/// artifacts and the way to reconcile them is the difference between a
/// diagnosis and a wall of text.
pub fn duplicate_classes(archives: &[PathBuf]) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for archive in archives {
        let Ok(file) = fs::File::open(archive) else {
            continue;
        };
        let Ok(mut zip) = zip::ZipArchive::new(file) else {
            // A jar that is not a zip is not a classpath entry we can read.
            continue;
        };
        for i in 0..zip.len() {
            let Ok(entry) = zip.by_index(i) else { continue };
            let name = entry.name();
            // A `module-info.class`, and every multi-release overlay under
            // `META-INF/versions/`, is a JPMS descriptor that shadows within its
            // own jar rather than a class to dex. d8 ignores them, so reporting
            // them would fail every build whose closure has two such jars.
            if !name.ends_with(".class")
                || name.starts_with("META-INF/")
                || name == "module-info.class"
            {
                continue;
            }
            // `.../<artifact>/<version>/jars/<name>.jar`, so two levels up is the
            // artifact. Naming the version instead would make the report
            // unreadable, which is the thing being fixed.
            let library = archive
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| archive.display().to_string());
            owners
                .entry(name.to_owned())
                .or_default()
                .insert(format!("{library} ({})", file_label(archive)));
        }
    }
    Ok(owners
        .into_iter()
        .filter(|(_, libs)| libs.len() > 1)
        .map(|(class, libs)| (class, libs.into_iter().collect()))
        .collect())
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

/// The `android:minSdkVersion` an AAR's own manifest requires.
fn manifest_min_sdk(xml: &str) -> Option<u32> {
    let open = xml.find("<uses-sdk")?;
    let tag = &xml[open..];
    let end = tag.find('>')?;
    let attr = tag[..end].find("minSdkVersion=")? + "minSdkVersion=".len();
    let rest = &tag[attr..];
    let quote = rest.chars().next()?;
    if !matches!(quote, '"' | '\'') {
        return None;
    }
    rest[1..]
        .find(quote)
        .and_then(|end| rest[1..1 + end].parse().ok())
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
fn cached_archive(
    dir: &Path,
    coordinates: &Coordinates,
    classifier: Option<&str>,
) -> Option<PathBuf> {
    let stem = match classifier {
        Some(classifier) => format!("{}-{classifier}", archive_stem(coordinates)),
        None => archive_stem(coordinates),
    };
    [".aar", ".jar"]
        .into_iter()
        .map(|ext| dir.join(format!("{stem}{ext}")))
        .find(|p| p.is_file())
}

fn archive_stem(coordinates: &Coordinates) -> String {
    format!("{}-{}", coordinates.artifact, coordinates.version)
}

/// Re-reads an already-extracted library from the cache, so a cached artifact
/// costs no network access and no re-extraction.
fn read_resolved(
    dir: &Path,
    coordinates: Coordinates,
    classifier: Option<&str>,
) -> Option<ResolvedLib> {
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
    let library_manifest = cached_archive(dir, &coordinates, classifier)
        .and_then(|archive| read_archive_manifest(&archive));
    let package = library_manifest
        .as_deref()
        .and_then(manifest_package)
        .filter(|p| !p.is_empty());
    let min_sdk_version = library_manifest.as_deref().and_then(manifest_min_sdk);
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
        min_sdk_version,
        classifier: classifier.map(str::to_owned),
    })
}

/// Fetches (or reuses) one `android_libs` entry: its jars, `res/` tree and
/// manifest, all cached under the artifact directory.
///
/// This fetches one artifact. Use [`resolve_libs`] to walk the dependency graph
/// as well.
pub fn ensure_lib(gav: &str) -> Result<ResolvedLib, NdkError> {
    ensure_lib_as(gav, None, None)
}

/// As [`ensure_lib`], but fetching `extension` when the module's metadata named
/// one. A POM can only say "aar or jar", so `None` probes for an AAR first and
/// then a jar; Gradle Module Metadata says which, and taking it is what keeps a
/// stub jar published under a multiplatform module's own coordinates off the
/// classpath.
pub fn ensure_lib_as(
    gav: &str,
    extension: Option<&str>,
    classifier: Option<&str>,
) -> Result<ResolvedLib, NdkError> {
    ensure_lib_inner(gav, extension, classifier, false)?.ok_or_else(|| NdkError::MavenLibFailed {
        gav: gav.to_owned(),
        reason: format!("no `.aar` or `.jar` published for {gav}"),
    })
}

/// As [`ensure_lib_as`], but for a module whose archive may legitimately be
/// absent. `Ok(None)` then means the repository publishes none, which is not a
/// failure for an optional artifact.
fn ensure_lib_inner(
    gav: &str,
    extension: Option<&str>,
    classifier: Option<&str>,
    optional: bool,
) -> Result<Option<ResolvedLib>, NdkError> {
    let coordinates = Coordinates::parse(gav).map_err(|reason| NdkError::MavenLibFailed {
        gav: gav.to_owned(),
        reason,
    })?;
    let dir = artifact_dir(&coordinates);
    let marker = marker_path(&coordinates);

    if !fetch_forced()
        && std::fs::read_to_string(&marker).is_ok_and(|m| m.trim() == coordinates.gav())
        && let Some(lib) = read_resolved(&dir, coordinates.clone(), classifier)
    {
        return Ok(Some(lib));
    }

    let fail = |reason: String| NdkError::MavenLibFailed {
        gav: gav.to_owned(),
        reason,
    };

    if fetch_disabled() && !optional {
        return Err(fail(format!(
            "not in {} and fetching is disabled; set CARGO_RAPK_FETCH_MAVEN, or unset \
             CARGO_RAPK_NO_FETCH_MAVEN",
            dir.display()
        )));
    }

    let wanted = extension.unwrap_or("aar");
    let (archive, is_aar): (Option<PathBuf>, bool) = match fetch_artifact_inner(
        &dir,
        &coordinates.group,
        &coordinates.artifact,
        &coordinates.version,
        wanted,
        classifier,
        false,
    ) {
        Ok(found) => (found, wanted == "aar"),
        Err(first_err) => {
            // A POM names no archive, so a jar is tried when no AAR answers.
            if wanted == "jar" {
                return if optional {
                    Ok(None)
                } else {
                    Err(fail(first_err))
                };
            }
            match fetch_artifact_inner(
                &dir,
                &coordinates.group,
                &coordinates.artifact,
                &coordinates.version,
                "jar",
                classifier,
                false,
            ) {
                Ok(found) => (found, false),
                // A repository that publishes neither archive is not a failure
                // for an artifact that was only ever optional, which is what
                // Gradle's `optionalArtifact` means by offering one. Treating it
                // as a failure fails every closure holding a BOM, since a BOM
                // publishes a POM and nothing else.
                Err(_) if optional => (None, false),
                _ => return Err(fail(first_err)),
            }
        }
    };
    let Some(archive) = archive else {
        if optional {
            return Ok(None);
        }
        return Err(fail(format!(
            "no `.aar` or `.jar` published for {gav} on {}",
            repo_base(&coordinates.group)
        )));
    };

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

    let min_sdk_version = library_manifest.as_deref().and_then(manifest_min_sdk);
    let manifest = library_manifest.filter(|xml| {
        manifest_contributions(xml)
            .map(|c| !c.is_empty())
            .unwrap_or(true)
    });

    // The marker alone identifies the cache entry; the manifest is re-read from
    // the archive, so nothing else needs persisting.
    let _ = std::fs::remove_file(dir.join(".cargo-rapk-lib-meta"));
    std::fs::write(&marker, coordinates.gav()).map_err(|e| fail(e.to_string()))?;
    Ok(Some(ResolvedLib {
        coordinates,
        jars,
        resources,
        package,
        manifest,
        min_sdk_version,
        classifier: classifier.map(str::to_owned),
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn credentials_go_to_curl_on_stdin_rather_than_the_command_line() {
        // `/proc/<pid>/cmdline` is world-readable, so an argument is a published
        // secret. `curl --config -` is the way to keep one out of it.
        let repository = Repository {
            url: "https://example.invalid/maven".to_owned(),
            username: Some("reader".to_owned()),
            password: Some("hunter2".to_owned()),
            token: Some("t0ken".to_owned()),
            ..Repository::default()
        };
        let config = repository.curl_config().expect("there are credentials");
        assert!(!config.contains("t0ken "), "no trailing space: {config}");
        assert_eq!(
            config,
            "header = \"Authorization: Bearer t0ken\"\nuser = \"reader:hunter2\"\n"
        );
    }

    #[test]
    fn a_credential_containing_a_quote_survives_the_configuration_parser() {
        // curl reads a double-quoted value in which a backslash escapes the next
        // character, so an unescaped quote would end the value early and send
        // the rest of the token as a separate, malformed line.
        let repository = Repository {
            url: "https://example.invalid/maven".to_owned(),
            token: Some("a\"b\\c".to_owned()),
            ..Repository::default()
        };
        assert_eq!(
            repository.curl_config().expect("there is a token"),
            "header = \"Authorization: Bearer a\\\"b\\\\c\"\n"
        );
        assert_eq!(
            quote_curl_config(r#"plain"#),
            "\"plain\"".to_owned(),
            "a value with nothing to escape is quoted and nothing more"
        );
    }

    #[test]
    fn a_repository_with_no_credentials_asks_for_no_configuration() {
        assert_eq!(
            Repository {
                url: "https://example.invalid/maven".to_owned(),
                ..Repository::default()
            }
            .curl_config(),
            None
        );
    }

    #[test]
    fn an_android_libs_entry_may_name_the_role_it_plays() {
        assert_eq!(parse_entry("g:a:1.0").expect("an entry").role, Role::Auto);
        assert_eq!(
            parse_entry("g:a:1.0!platform").expect("a platform").role,
            Role::Platform
        );
        assert_eq!(
            parse_entry("g:a:1.0 ! library").expect("a library").role,
            Role::Library
        );
        assert_eq!(
            parse_entry("g:a:1.0!platform")
                .expect("a platform")
                .coordinates
                .gav(),
            "g:a:1.0",
            "the role is not part of the coordinates"
        );
        assert!(parse_entry("g:a:1.0!bill").is_err(), "no other role");
    }

    use super::*;

    fn repository(url: &str, include: &[&str], exclude: &[&str]) -> Repository {
        Repository {
            url: url.to_owned(),
            include_groups: include.iter().map(|g| (*g).to_owned()).collect(),
            exclude_groups: exclude.iter().map(|g| (*g).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_group_prefix_selects_which_repositories_serve_it() {
        let google = repository(GOOGLE_BASE, &[], &[]);
        let central = repository(CENTRAL_BASE, &[], &[]);
        // An empty include list serves everything, which is what the two
        // defaults do: a group prefix is a guess, and `com.google.firebase`
        // shows how wrong the guess can be.
        assert!(google.accepts("androidx.appcompat"));
        assert!(google.accepts("com.google.firebase"));
        assert!(central.accepts("com.google.firebase"));

        let internal = repository("https://nexus", &["com.mycorp"], &[]);
        assert!(internal.accepts("com.mycorp.sdk"));
        assert!(!internal.accepts("org.foo"));
        assert!(repository("https://nexus", &[], &["com.mycorp.legacy"]).accepts("com.mycorp.sdk"));
        assert!(
            !repository("https://nexus", &[], &["com.mycorp.legacy"])
                .accepts("com.mycorp.legacy.old")
        );
    }

    #[test]
    fn a_classifier_is_part_of_the_artifact_name() {
        assert_eq!(
            artifact_url_classified(GOOGLE_BASE, "g", "a", "1.0", "jar", None),
            format!("{GOOGLE_BASE}/g/a/1.0/a-1.0.jar")
        );
        assert_eq!(
            artifact_url_classified(CENTRAL_BASE, "g", "a", "1.0", "jar", Some("linux-x86_64")),
            format!("{CENTRAL_BASE}/g/a/1.0/a-1.0-linux-x86_64.jar")
        );
    }

    #[test]
    fn an_unterminated_variable_is_left_alone_rather_than_repeated() {
        // `${` with no closing brace is a typo, not a template: leaving it as
        // written names what is wrong, whereas repeating the prefix hides it.
        for (input, expected) in [
            ("abc${", "abc${"),
            ("a/${X}/b", "a/${X}/b"),
            ("https://${HOST/m2", "https://${HOST/m2"),
            ("plain", "plain"),
            ("", ""),
        ] {
            assert_eq!(
                Repository {
                    url: input.to_owned(),
                    ..Default::default()
                }
                .resolved()
                .url,
                expected,
                "`{input}`"
            );
        }
    }

    #[test]
    fn only_a_url_is_required_of_a_repository() {
        // The whole `Cargo.toml` is deserialized in one pass, so a missing
        // optional field would otherwise take every other setting with it.
        let parsed: Repository =
            toml::from_str("url = \"https://nexus\"").expect("url alone parses");
        assert_eq!(parsed.url, "https://nexus");
        assert!(parsed.token.is_none());
        assert!(parsed.include_groups.is_empty());
        assert!(toml::from_str::<Repository>("token = \"x\"").is_err());
    }

    /// A jar at the layout `ensure_lib` extracts to, since the report names the
    /// artifact directory rather than the version.
    fn extracted_jar(root: &Path, artifact: &str, version: &str, entries: &[&str]) -> PathBuf {
        let dir = root.join(artifact).join(version).join("jars");
        std::fs::create_dir_all(&dir).expect("cache layout");
        let path = dir.join(format!("{artifact}-{version}.jar"));
        let file = std::fs::File::create(&path).expect("jar is created");
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for entry in entries {
            zip.start_file(*entry, options).expect("entry is added");
            use std::io::Write;
            zip.write_all(b"x").expect("entry is written");
        }
        zip.finish().expect("jar is finished");
        path
    }

    #[test]
    fn only_a_program_class_counts_as_a_duplicate() {
        // `module-info.class` sits at the jar root and a multi-release overlay
        // under `META-INF/versions/`. Both are JPMS descriptors that `d8` does
        // not dex, and reporting either would fail a closure with two such jars.
        let root = std::env::temp_dir().join(format!("cargo-rapk-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let descriptors = [
            "module-info.class",
            "META-INF/versions/9/module-info.class",
            "com/example/Real.class",
        ];
        let a = extracted_jar(&root, "kotlin-stdlib", "1.8.22", &descriptors);
        let b = extracted_jar(&root, "gson", "2.8.9", &descriptors);
        let duplicates = duplicate_classes(&[a, b]).expect("archives read");
        assert_eq!(
            duplicates,
            vec![(
                "com/example/Real.class".to_owned(),
                vec![
                    "gson (jars/gson-2.8.9.jar)".to_owned(),
                    "kotlin-stdlib (jars/kotlin-stdlib-1.8.22.jar)".to_owned(),
                ]
            )],
            "only the real class is a duplicate, and the owners name the artifact"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_archive_is_skipped_rather_than_fatal() {
        assert!(
            duplicate_classes(&[PathBuf::from("/nonexistent.jar")])
                .expect("a missing file is not a failure")
                .is_empty()
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
