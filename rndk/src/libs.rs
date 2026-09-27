use crate::error::NdkError;
use crate::ndk::Ndk;
use crate::target::Target;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The one library the NDK ships that is not on the device and therefore always
/// has to be packaged when something links against it.
const CXX_SHARED: &str = "libc++_shared.so";

/// Where a library was found. Ordering is precedence: a candidate from a higher
/// variant always wins a name collision against a lower one, so the main
/// library outranks an explicitly named file, which outranks a Cargo
/// link-search path, which outranks the NDK sysroot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LibSource {
    /// The crate being packaged.
    MainLib,
    /// A file named by `runtime_libs`.
    Explicit,
    /// A Cargo `rustc-link-search` path.
    SearchPath,
    /// The NDK sysroot.
    Sysroot,
}

impl Ord for LibSource {
    fn cmp(&self, other: &Self) -> Ordering {
        precedence(*self).cmp(&precedence(*other))
    }
}

impl PartialOrd for LibSource {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

const fn precedence(source: LibSource) -> u8 {
    match source {
        LibSource::MainLib => 3,
        LibSource::Explicit => 2,
        LibSource::SearchPath => 1,
        LibSource::Sysroot => 0,
    }
}

struct Candidate {
    source: LibSource,
    path: PathBuf,
}

/// Every file that could satisfy each library name, and the rule for picking one.
///
/// The choice is a pure function of the candidates offered for a name, so it
/// cannot depend on the order they were discovered in. A name resolves to the
/// candidate from the highest-ranked source; within one source, files that agree
/// byte for byte are one library and the lowest path represents them, while
/// files that disagree have no principled winner and are an error.
#[derive(Default)]
pub struct LibCandidates {
    entries: BTreeMap<String, Vec<Candidate>>,
}

impl LibCandidates {
    fn offer(&mut self, name: String, source: LibSource, path: &Path) -> Result<(), NdkError> {
        let path = dunce::canonicalize(path)?;
        let candidates = self.entries.entry(name).or_default();
        if !candidates.iter().any(|c| c.path == path) {
            candidates.push(Candidate { source, path });
        }
        Ok(())
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// The single file that represents `name`, if any candidate does.
    pub fn pick(&self, name: &str) -> Result<Option<PathBuf>, NdkError> {
        let candidates = self.entries.get(name).map_or(&[][..], |c| c.as_slice());
        let Some(best) = candidates.iter().map(|c| c.source).max() else {
            return Ok(None);
        };

        let mut paths = candidates
            .iter()
            .filter(|c| c.source == best)
            .map(|c| c.path.clone())
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();

        let Some((winner, rest)) = paths.split_first() else {
            return Ok(None);
        };
        for other in rest {
            if !same_contents(winner, other)? {
                return Err(NdkError::AmbiguousLibrary {
                    name: name.to_owned(),
                    candidates: paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n  "),
                });
            }
        }

        Ok(Some(winner.clone()))
    }
}

/// Resolves the closure of shared libraries to package, one file per name.
///
/// Resolution runs to a fixed point: a name is re-picked whenever a later
/// candidate outranks the current winner, and the new winner's own dependencies
/// are then discovered in turn. Candidates are only ever added, so the winner
/// for a name can only move up the precedence order and the walk terminates.
pub struct LibResolver {
    readelf: PathBuf,
    search_paths: Vec<PathBuf>,
    sysroot_paths: Vec<PathBuf>,
    /// Names the platform provides, which are never packaged.
    on_device: BTreeSet<String>,
    candidates: LibCandidates,
    missing: BTreeSet<String>,
}

impl LibResolver {
    pub fn new(
        ndk: &Ndk,
        target: Target,
        min_sdk_version: u32,
        search_paths: Vec<PathBuf>,
    ) -> Result<Self, NdkError> {
        let sysroot_paths = vec![
            ndk.sysroot_platform_lib_dir(target, min_sdk_version)?,
            ndk.sysroot_lib_dir(target)?,
        ];

        let mut on_device = BTreeSet::new();
        for dir in &sysroot_paths {
            for name in list_libs(dir)? {
                if name != CXX_SHARED {
                    on_device.insert(name);
                }
            }
        }

        Ok(Self {
            readelf: ndk.toolchain_bin("readelf", target)?,
            search_paths,
            sysroot_paths,
            on_device,
            candidates: LibCandidates::default(),
            missing: BTreeSet::new(),
        })
    }

    /// Registers a library the caller has already decided to package.
    pub fn register(&mut self, source: LibSource, path: &Path) -> Result<(), NdkError> {
        self.candidates
            .offer(shared_object_name(path)?, source, path)
    }

    /// Registers every library in an ABI directory, such as the one
    /// `runtime_libs` points at.
    pub fn register_dir(&mut self, source: LibSource, dir: &Path) -> Result<(), NdkError> {
        let entries = std::fs::read_dir(dir).map_err(|e| NdkError::IoPathError(dir.into(), e))?;
        let mut paths = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "so"))
            .collect::<Vec<_>>();
        paths.sort();

        for path in paths {
            self.register(source, &path)?;
        }
        Ok(())
    }

    /// The single file to package for each library name, keyed by the name it
    /// takes inside the APK.
    ///
    /// Libraries the platform provides only reach this table when the caller
    /// registered one explicitly, which is taken as intent to package it.
    pub fn resolve(&mut self) -> Result<BTreeMap<String, PathBuf>, NdkError> {
        let mut resolved: BTreeMap<String, PathBuf> = BTreeMap::new();

        loop {
            let mut changed = false;
            for name in self.candidates.names().cloned().collect::<Vec<_>>() {
                let Some(path) = self.candidates.pick(&name)? else {
                    continue;
                };
                if resolved.get(&name) == Some(&path) {
                    continue;
                }
                resolved.insert(name, path.clone());
                changed = true;
                self.discover(&path)?;
            }
            if !changed {
                break;
            }
        }

        for missing in &self.missing {
            eprintln!("Shared library \"{missing}\" not found.");
        }

        Ok(resolved)
    }

    /// Offers a candidate for every `DT_NEEDED` entry of a packaged library.
    ///
    /// This is the only place the platform's own libraries are filtered out, so
    /// a library the caller registered explicitly is still packaged even when
    /// the device also provides it.
    fn discover(&mut self, path: &Path) -> Result<(), NdkError> {
        for need in self.needed(path)? {
            if self.on_device.contains(&need) || self.candidates.contains(&need) {
                continue;
            }
            if !self.search(&need)? {
                self.missing.insert(need);
            }
        }
        Ok(())
    }

    /// Offers every file matching `need` from the highest-ranked source that has
    /// any, so that a same-source disagreement stays visible to
    /// [`LibCandidates::pick`].
    fn search(&mut self, need: &str) -> Result<bool, NdkError> {
        let mut found: Vec<PathBuf> = Vec::new();
        let mut source = LibSource::Sysroot;
        for (candidate_source, dirs) in [
            (LibSource::SearchPath, &self.search_paths),
            (LibSource::Sysroot, &self.sysroot_paths),
        ] {
            found = dirs
                .iter()
                .map(|dir| dir.join(need))
                .filter(|path| path.is_file())
                .filter_map(|path| dunce::canonicalize(path).ok())
                .collect();
            if !found.is_empty() {
                source = candidate_source;
                break;
            }
        }

        if found.is_empty() {
            return Ok(false);
        }
        for path in found {
            self.candidates.offer(need.to_owned(), source, &path)?;
        }
        Ok(true)
    }

    /// The `DT_NEEDED` entries of a library, sorted so that the walk is
    /// reproducible.
    fn needed(&self, path: &Path) -> Result<Vec<String>, NdkError> {
        let mut readelf = Command::new(&self.readelf);
        let output = readelf.arg("-d").arg(path).output()?;
        if !output.status.success() {
            return Err(NdkError::CmdFailed(Box::new(readelf)));
        }
        Ok(output
            .stdout
            .split(|b| *b == b'\n')
            .filter(|line| line.windows(9).any(|w| w == b" (NEEDED)"))
            .filter_map(|line| {
                let line = String::from_utf8_lossy(line);
                let lib = line.split("Shared library: [").nth(1)?;
                Some(lib.split(']').next()?.to_owned())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }
}

fn shared_object_name(path: &Path) -> Result<String, NdkError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.ends_with(".so"))
        .map(str::to_owned)
        .ok_or_else(|| NdkError::PathNotFound(path.into()))
}

fn list_libs(dir: &Path) -> Result<BTreeSet<String>, NdkError> {
    let entries = std::fs::read_dir(dir).map_err(|e| NdkError::IoPathError(dir.into(), e))?;
    Ok(entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| shared_object_name(&entry.path()).ok())
        .collect())
}

/// Exact content comparison, so that a library vendored into several places is
/// recognised as one library rather than reported as a disagreement.
fn same_contents(a: &Path, b: &Path) -> Result<bool, NdkError> {
    let (mut a, mut b) = (open(a)?, open(b)?);
    if a.get_ref().metadata()?.len() != b.get_ref().metadata()?.len() {
        return Ok(false);
    }

    let (mut a_buf, mut b_buf) = ([0u8; 64 * 1024], [0u8; 64 * 1024]);
    loop {
        let read = read_full(&mut a, &mut a_buf)?;
        let read_b = read_full(&mut b, &mut b_buf)?;
        if read != read_b || a_buf[..read] != b_buf[..read_b] {
            return Ok(false);
        }
        if read == 0 {
            return Ok(true);
        }
    }
}

fn open(path: &Path) -> Result<BufReader<File>, NdkError> {
    File::open(path)
        .map(BufReader::new)
        .map_err(|e| NdkError::IoPathError(path.into(), e))
}

fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize, NdkError> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(case: &str, tag: &str, contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("rndk-libs-test")
            .join(case)
            .join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("libtest.so");
        std::fs::write(&path, contents).unwrap();
        dunce::canonicalize(path).unwrap()
    }

    #[test]
    fn higher_source_wins_regardless_of_offer_order() {
        let search = lib("order", "search", b"from search path");
        let explicit = lib("order", "explicit", b"from runtime_libs");

        let mut candidates = LibCandidates::default();
        candidates
            .offer("libbar.so".to_string(), LibSource::SearchPath, &search)
            .unwrap();
        candidates
            .offer("libbar.so".to_string(), LibSource::Explicit, &explicit)
            .unwrap();
        assert_eq!(
            candidates.pick("libbar.so").unwrap(),
            Some(explicit.clone())
        );

        let mut reversed = LibCandidates::default();
        reversed
            .offer("libbar.so".to_string(), LibSource::Explicit, &explicit)
            .unwrap();
        reversed
            .offer("libbar.so".to_string(), LibSource::SearchPath, &search)
            .unwrap();
        assert_eq!(reversed.pick("libbar.so").unwrap(), Some(explicit));
    }

    #[test]
    fn identical_copies_in_one_source_collapse_to_the_lowest_path() {
        let a = lib("identical", "a", b"identical bytes");
        let b = lib("identical", "b", b"identical bytes");
        let lowest = a.clone().min(b.clone());

        let mut candidates = LibCandidates::default();
        for path in [&a, &b] {
            candidates
                .offer("libtest.so".to_string(), LibSource::SearchPath, path)
                .unwrap();
        }
        assert_eq!(candidates.pick("libtest.so").unwrap(), Some(lowest));
    }

    #[test]
    fn differing_copies_in_one_source_are_rejected() {
        let a = lib("conflict", "a", b"first build");
        let b = lib("conflict", "b", b"second build");

        let mut candidates = LibCandidates::default();
        for path in [&a, &b] {
            candidates
                .offer("libtest.so".to_string(), LibSource::SearchPath, path)
                .unwrap();
        }
        assert!(matches!(
            candidates.pick("libtest.so"),
            Err(NdkError::AmbiguousLibrary { ref name, .. }) if name == "libtest.so"
        ));
    }
}
