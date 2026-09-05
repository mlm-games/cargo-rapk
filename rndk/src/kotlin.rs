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
//! Missing compiler is fetched into the cache, Maven Central first (same
//! artifacts the Gradle plugin resolves), dist zip as fallback if no java.
//! `CARGO_RAPK_KOTLIN_VERSION` (default `2.2.10`), `CARGO_RAPK_KOTLIN_SHA256`,
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
            let v = normalize_pin(&v);
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

fn parse_triple(s: &str) -> Option<(u32, u32, u32)> {
    let mut nums = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty());
    Some((nums.next()?.parse().ok()?, nums.next()?.parse().ok()?, {
        let patch: String = nums
            .next()?
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        patch.parse().ok()?
    }))
}

/// First `x.y.z` in `s` (e.g. `kotlinc-jvm 2.4.10 (JRE ...)`).
fn first_triple(s: &str) -> Option<String> {
    for tok in s.split_whitespace() {
        let t = tok.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if t.chars().filter(|c| *c == '.').count() >= 2 && parse_triple(t).is_some() {
            return parse_triple(t).map(|(a, b, c)| format!("{a}.{b}.{c}"));
        }
    }
    None
}

fn normalize_pin(pin: &str) -> String {
    pin.trim()
        .trim_matches('"')
        .trim_start_matches('v')
        .to_string()
}

/// Version reported by a kotlinc binary, if runnable.
pub fn probed_kotlinc_version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("-version").output().ok()?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    first_triple(&combined)
}

fn pin_matches_bin(pin: &str, bin: &Path) -> bool {
    match probed_kotlinc_version(bin) {
        Some(v) if v == normalize_pin(pin) => true,
        Some(v) => {
            log::warn!(
                "ignoring kotlinc {} (version {v}, want {pin})",
                bin.display()
            );
            false
        }
        None => {
            log::warn!("could not probe {} version; ignoring", bin.display());
            false
        }
    }
}

/// Marker written next to a fetched compiler so cache hits need no JVM probe.
fn cache_marker() -> PathBuf {
    kotlin_cache_dir()
        .join("kotlinc")
        .join(".cargo-rapk-version")
}

fn cache_matches_pin(pin: &str) -> bool {
    match std::fs::read_to_string(cache_marker()) {
        Ok(v) if v.trim() == normalize_pin(pin) => true,
        Ok(_) => false,
        Err(_) => pin_matches_bin(pin, &expected_kotlinc_path()),
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
    if let Ok(p) = which::which("kotlinc")
        && pin_matches_bin(&kotlin_version(), &p)
    {
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
    if is_executable_file(&cached) && cache_matches_pin(&kotlin_version()) {
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
        "kotlinc not found (java_sources contain *.kt). Auto-fetch from Maven Central \
needs curl/wget + java, or install kotlinc {version}+, put it on PATH, \
or set CARGO_RAPK_KOTLINC / KOTLIN_HOME. Expected dist cache: {}. Maven cache: {}",
        cache.display(),
        maven_dir(version).display(),
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

// It is the same compiler that Gradle plugin resolves (`kotlin-compiler-embeddable`),

const MAVEN_BASE: &str = "https://repo.maven.apache.org/maven2";
const KOTLIN_CLI_MAIN: &str = "org.jetbrains.kotlin.cli.jvm.K2JVMCompiler";
const EMBEDDABLE_ARTIFACT: &str = "kotlin-compiler-embeddable";
const KOTLIN_GROUP: &str = "org.jetbrains.kotlin";

fn maven_dir(version: &str) -> PathBuf {
    kotlin_cache_dir()
        .join("maven")
        .join(normalize_pin(version))
}

fn maven_marker(version: &str) -> PathBuf {
    maven_dir(version).join(".cargo-rapk-version")
}

fn artifact_url(group: &str, artifact: &str, version: &str, ext: &str) -> String {
    format!(
        "{MAVEN_BASE}/{}/{artifact}/{version}/{artifact}-{version}.{ext}",
        group.replace('.', "/")
    )
}

#[derive(Debug, Clone)]
pub struct MavenToolchain {
    pub dir: PathBuf,
    pub jars: Vec<PathBuf>,
    pub stdlib: PathBuf,
}

fn sorted_jars(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut jars = Vec::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        let name = path.file_name()?.to_string_lossy().into_owned();
        if !name.ends_with(".jar") || name.contains("sources") || name.contains("javadoc") {
            continue;
        }
        jars.push(path);
    }
    jars.sort();
    if jars.is_empty() { None } else { Some(jars) }
}

fn find_stdlib(jars: &[PathBuf]) -> Option<PathBuf> {
    jars.iter()
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("kotlin-stdlib-"))
        })
        .cloned()
}

fn maven_has_compiler(jars: &[PathBuf]) -> bool {
    jars.iter().any(|p| {
        p.file_name().is_some_and(|n| {
            n.to_string_lossy()
                .starts_with(&format!("{EMBEDDABLE_ARTIFACT}-"))
        })
    })
}

/// Resolve a previously fetched Maven toolchain (no network).
pub fn resolve_maven() -> Option<MavenToolchain> {
    let version = kotlin_version();
    if std::fs::read_to_string(maven_marker(&version)).ok()?.trim() != normalize_pin(&version) {
        return None;
    }
    let dir = maven_dir(&version);
    let jars = sorted_jars(&dir)?;
    if !maven_has_compiler(&jars) {
        return None;
    }
    let stdlib = find_stdlib(&jars)?;
    Some(MavenToolchain { dir, jars, stdlib })
}

impl MavenToolchain {
    fn java_bin() -> Result<PathBuf, String> {
        if let Ok(p) = which::which("java") {
            return Ok(p);
        }
        if let Ok(home) = std::env::var("JAVA_HOME") {
            #[cfg(target_os = "windows")]
            let c = PathBuf::from(home).join("bin").join("java.exe");
            #[cfg(not(target_os = "windows"))]
            let c = PathBuf::from(home).join("bin").join("java");
            if c.is_file() {
                return Ok(c);
            }
        }
        Err(
            "`java` not found on PATH (or JAVA_HOME); a JRE is required to run the Kotlin compiler"
                .into(),
        )
    }

    /// `java -cp <closure> K2JVMCompiler`.
    pub fn command(&self) -> Result<Command, NdkError> {
        let java = Self::java_bin().map_err(NdkError::KotlinJavaMissing)?;
        let sep = if cfg!(target_os = "windows") {
            ";"
        } else {
            ":"
        };
        let cp = self
            .jars
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(sep);
        let mut cmd = Command::new(java);
        cmd.arg("-cp").arg(cp).arg(KOTLIN_CLI_MAIN);
        cmd.env_remove(ENV_LEGACY_COMPILER);
        Ok(cmd)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MavenDep {
    group: String,
    artifact: String,
    version: String,
    no_descend: bool,
}

fn extract_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let body_start = match rest[start..].find('>') {
            Some(i) => start + i + 1,
            None => break,
        };
        let Some(end) = rest[body_start..].find(&close) else {
            break;
        };
        out.push(&rest[body_start..body_start + end]);
        rest = &rest[body_start + end + close.len()..];
    }
    out
}

fn extract_one(block: &str, tag: &str) -> Option<String> {
    extract_blocks(block, tag)
        .first()
        .map(|s| s.trim().to_string())
}

/// Parse `<dependencies>` from a POM: direct deps with `${prop}` substituted.
/// `no_descend` marks wildcard-excluded deps (their own POMs aren't followed).
fn parse_pom_deps(pom_xml: &str) -> Vec<MavenDep> {
    let mut props = std::collections::HashMap::new();
    for block in extract_blocks(pom_xml, "properties") {
        // `<properties><a>1</a><b>2</b></properties>`: split on '<'.
        // The last value has no trailing '<', so default to the whole rest.
        for chunk in block.split('<').skip(1) {
            if let Some((name, rest)) = chunk.split_once('>') {
                let value = rest.split_once('<').map(|(v, _)| v).unwrap_or(rest);
                if !name.starts_with('/') && !value.trim().is_empty() {
                    props.insert(name.to_string(), value.trim().to_string());
                }
            }
        }
    }
    let subst = |v: &str| {
        let mut v = v.trim().to_string();
        for (k, val) in &props {
            v = v.replace(&format!("${{{k}}}"), val);
        }
        v
    };
    let mut deps = Vec::new();
    for dep_block in extract_blocks(pom_xml, "dependencies")
        .iter()
        .flat_map(|b| extract_blocks(b, "dependency"))
    {
        let scope = extract_one(dep_block, "scope")
            .unwrap_or_else(|| "compile".to_string())
            .to_ascii_lowercase();
        if matches!(scope.as_str(), "test" | "provided" | "system") {
            continue;
        }
        if extract_one(dep_block, "optional").is_some_and(|o| o == "true") {
            continue;
        }
        let (Some(group), Some(artifact), Some(version)) = (
            extract_one(dep_block, "groupId"),
            extract_one(dep_block, "artifactId"),
            extract_one(dep_block, "version").map(|v| subst(&v)),
        ) else {
            continue;
        };
        let no_descend = extract_blocks(dep_block, "exclusions").iter().any(|ex| {
            extract_blocks(ex, "exclusion").iter().any(|e| {
                extract_one(e, "artifactId").is_some_and(|a| a == "*")
                    || extract_one(e, "groupId").is_some_and(|g| g == "*")
            })
        });
        deps.push(MavenDep {
            group,
            artifact,
            version,
            no_descend,
        });
    }
    deps
}

fn download_text(url: &str) -> Result<String, String> {
    let tmp = std::env::temp_dir().join(format!("cargo-rapk-pom-{}", std::process::id()));
    download_with_curl_or_wget(url, &tmp)?;
    let text = std::fs::read_to_string(&tmp).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&tmp);
    Ok(text)
}

fn verify_sha1(path: &Path, expected: &str) -> bool {
    let actual = file_digest_hex(path, "sha1sum", "1", 40);
    actual.is_some_and(|a| a == expected.trim().to_ascii_lowercase())
}

/// Download one Maven artifact + fail-closed `.sha1` check. Reuses verified files.
fn fetch_artifact(
    dir: &Path,
    group: &str,
    artifact: &str,
    version: &str,
    ext: &str,
) -> Result<Option<PathBuf>, String> {
    fetch_artifact_inner(dir, group, artifact, version, ext, false)
}

fn url_definitely_missing(url: &str) -> Option<bool> {
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

fn fetch_artifact_inner(
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

pub fn fetch_maven(version: &str) -> Result<MavenToolchain, NdkError> {
    let version = normalize_pin(version);
    let dir = maven_dir(&version);
    std::fs::create_dir_all(&dir).map_err(|e| NdkError::KotlinFetchFailed {
        version: version.clone(),
        reason: e.to_string(),
    })?;
    let fail = |reason: String| NdkError::KotlinFetchFailed {
        version: version.clone(),
        reason,
    };
    let root_pom = fetch_artifact(&dir, KOTLIN_GROUP, EMBEDDABLE_ARTIFACT, &version, "pom")
        .map_err(&fail)?
        .ok_or_else(|| fail("embeddable POM unpublished".into()))?;
    let root_xml = std::fs::read_to_string(&root_pom).map_err(|e| fail(e.to_string()))?;
    fetch_artifact(&dir, KOTLIN_GROUP, EMBEDDABLE_ARTIFACT, &version, "jar")
        .map_err(&fail)?
        .ok_or_else(|| fail("embeddable jar unpublished".into()))?;
    let mut seen = std::collections::HashSet::from([(
        KOTLIN_GROUP.to_string(),
        EMBEDDABLE_ARTIFACT.to_string(),
        version.clone(),
    )]);
    let mut depth1 = Vec::new();
    for dep in parse_pom_deps(&root_xml) {
        let key = (dep.group.clone(), dep.artifact.clone(), dep.version.clone());
        if seen.insert(key) {
            fetch_artifact(&dir, &dep.group, &dep.artifact, &dep.version, "jar")
                .map_err(&fail)?
                .ok_or_else(|| fail(format!("{} unpublished", dep.artifact)))?;
        }
        if !dep.no_descend {
            depth1.push(dep);
        }
    }
    for dep in depth1 {
        let pom_path = match fetch_artifact_inner(
            &dir,
            &dep.group,
            &dep.artifact,
            &dep.version,
            "pom",
            true,
        ) {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("skipping transitive POM for {}: {e}", dep.artifact);
                continue;
            }
        };
        let xml = std::fs::read_to_string(&pom_path).map_err(|e| fail(e.to_string()))?;
        for t in parse_pom_deps(&xml) {
            let key = (t.group.clone(), t.artifact.clone(), t.version.clone());
            if seen.insert(key)
                && fetch_artifact_inner(&dir, &t.group, &t.artifact, &t.version, "jar", true)
                    .map_err(&fail)?
                    .is_none()
            {
                log::warn!(
                    "transitive {}-{} not published; compiler runs without it",
                    t.artifact,
                    t.version
                );
            }
        }
    }
    let _ = std::fs::write(maven_marker(&version), &version);
    resolve_maven().ok_or_else(|| {
        fail("fetch succeeded but toolchain incomplete (embeddable or stdlib jar missing)".into())
    })
}

/// Which Kotlin toolchain to use, and where it came from.
pub enum KotlinToolchain {
    /// Classic `kotlinc` launcher script (explicit install, PATH, or dist cache).
    Script(PathBuf),
    /// Maven-Central closure run as `java -cp … K2JVMCompiler`.
    Maven(MavenToolchain),
}

impl KotlinToolchain {
    pub fn command(&self) -> Result<Command, NdkError> {
        match self {
            KotlinToolchain::Script(p) => Ok(kotlinc_command(p)),
            KotlinToolchain::Maven(m) => m.command(),
        }
    }

    pub fn stdlib_jar(&self) -> PathBuf {
        match self {
            KotlinToolchain::Script(_) => expected_stdlib_path(),
            KotlinToolchain::Maven(m) => m.stdlib.clone(),
        }
    }
}

/// Resolve the toolchain: explicit script, Maven cache, PATH script
/// (version-checked), dist cache (version-checked). No network.
pub fn resolve_toolchain() -> Option<KotlinToolchain> {
    if !fetch_forced() {
        if let Some(p) = resolve_kotlinc_path() {
            return Some(KotlinToolchain::Script(p));
        }
        if let Some(m) = resolve_maven() {
            return Some(KotlinToolchain::Maven(m));
        }
    }
    None
}

/// Ensure a toolchain: resolve, else fetch Maven Central first (accepted
/// channel), dist zip as fallback. Only call when `.kt` files are present.
pub fn ensure_kotlin_toolchain() -> Result<KotlinToolchain, NdkError> {
    if let Some(t) = resolve_toolchain() {
        return Ok(t);
    }
    let version = kotlin_version();
    if fetch_disabled() {
        return Err(NdkError::CmdNotFound(install_instructions(&version)));
    }
    log::info!("fetching Kotlin v{version} from Maven Central into cache");
    match fetch_maven(&version) {
        Ok(m) => return Ok(KotlinToolchain::Maven(m)),
        Err(e) => log::warn!("maven fetch failed ({e}); falling back to dist zip"),
    }
    log::info!("fetching Kotlin v{version} dist zip into cache");
    match fetch_kotlin_into_cache(&version) {
        Ok(()) => {}
        Err(e) => {
            return Err(NdkError::CmdNotFound(format!(
                "{e}. {}",
                install_instructions(&version)
            )));
        }
    }
    resolve_toolchain().ok_or_else(|| NdkError::CmdNotFound(install_instructions(&version)))
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

fn file_sha256_hex(path: &Path) -> Option<String> {
    file_digest_hex(path, "sha256sum", "256", 64)
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
    let _ = std::fs::write(cache_marker(), normalize_pin(version));
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

    #[test]
    fn maven_artifact_urls() {
        assert_eq!(
            artifact_url("org.jetbrains.kotlin", "kotlin-stdlib", "2.2.10", "jar"),
            "https://repo.maven.apache.org/maven2/org/jetbrains/kotlin/kotlin-stdlib/2.2.10/kotlin-stdlib-2.2.10.jar"
        );
        assert_eq!(
            artifact_url(
                "org.jetbrains.kotlinx",
                "kotlinx-coroutines-core-jvm",
                "1.8.0",
                "pom"
            ),
            "https://repo.maven.apache.org/maven2/org/jetbrains/kotlinx/kotlinx-coroutines-core-jvm/1.8.0/kotlinx-coroutines-core-jvm-1.8.0.pom"
        );
    }

    #[test]
    fn parses_pom_closure() {
        let pom = r#"<?xml version="1.0" encoding="UTF-8"?>
<project>
  <properties><coroutines.version>1.8.0</coroutines.version></properties>
  <dependencies>
    <dependency>
      <groupId>org.jetbrains.kotlin</groupId>
      <artifactId>kotlin-stdlib</artifactId>
      <version>2.2.10</version>
      <scope>runtime</scope>
    </dependency>
    <dependency>
      <groupId>org.jetbrains.kotlin</groupId>
      <artifactId>kotlin-reflect</artifactId>
      <version>1.6.10</version>
      <scope>runtime</scope>
      <exclusions><exclusion><groupId>*</groupId><artifactId>*</artifactId></exclusion></exclusions>
    </dependency>
    <dependency>
      <groupId>org.jetbrains.kotlinx</groupId>
      <artifactId>kotlinx-coroutines-core-jvm</artifactId>
      <version>${coroutines.version}</version>
    </dependency>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
      <version>4.13</version>
      <scope>test</scope>
    </dependency>
  </dependencies>
</project>"#;
        let deps = parse_pom_deps(pom);
        assert_eq!(deps.len(), 3);
        assert!(
            deps.iter()
                .any(|d| d.artifact == "kotlin-stdlib" && !d.no_descend)
        );
        let reflect = deps
            .iter()
            .find(|d| d.artifact == "kotlin-reflect")
            .unwrap();
        assert!(reflect.no_descend);
        let cor = deps
            .iter()
            .find(|d| d.artifact == "kotlinx-coroutines-core-jvm")
            .unwrap();
        assert_eq!(cor.version, "1.8.0");
    }
}
