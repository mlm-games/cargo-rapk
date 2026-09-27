//! Resolution of `rustflags` from Cargo configuration files.
//!
//! [`crate::cargo::cargo_ndk`] has to export `CARGO_ENCODED_RUSTFLAGS` to get
//! its linker arguments into transitive `cdylib` builds, and Cargo gives that
//! environment variable precedence over every configuration file. Resolving the
//! configuration files here is what keeps `build.rustflags` and
//! `target.<triple>.rustflags` from being silently dropped.
//!
//! The precedence and merging follow the Cargo book: `target.<triple>` and
//! `target.<cfg>` entries win over `build.rustflags`, and arrays are joined
//! across the configuration hierarchy with deeper files taking precedence.
//! `target.<cfg>` entries cannot be evaluated without the target's full
//! `rustc --print cfg` set, so their presence is reported instead.

use std::path::{Path, PathBuf};

const MAX_INCLUDE_DEPTH: usize = 32;

/// `rustflags` that Cargo would have taken from configuration files for
/// `triple`, or an empty vector when it defines none.
pub fn config_rustflags(triple: &str) -> Vec<String> {
    let mut build = Vec::new();
    let mut target = Vec::new();
    let mut cfgs = Vec::new();

    for file in config_files() {
        collect(&file, triple, &mut build, &mut target, &mut cfgs, 0);
    }

    if !cfgs.is_empty() {
        eprintln!(
            "warning: ignoring `target.<cfg>.rustflags` in the Cargo configuration ({}); \
             `cfg()` matching is not evaluated, set `RUSTFLAGS` instead",
            cfgs.join(", ")
        );
    }

    if target.is_empty() { build } else { target }
}

/// The configuration files Cargo would read, lowest precedence first.
fn config_files() -> Vec<PathBuf> {
    let mut files = Vec::new();

    if let Some(cargo_home) = cargo_home()
        && let Some(path) = config_file(&cargo_home)
    {
        files.push(path);
    }

    let Ok(cwd) = std::env::current_dir() else {
        return files;
    };
    let mut ancestors = cwd.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for dir in ancestors {
        if let Some(path) = config_file(&dir.join(".cargo")) {
            files.push(path);
        }
    }

    files
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")))
}

/// Cargo prefers the extension-less `config` when both spellings exist.
fn config_file(dir: &Path) -> Option<PathBuf> {
    let legacy = dir.join("config");
    if legacy.is_file() {
        return Some(legacy);
    }
    let toml = dir.join("config.toml");
    toml.is_file().then_some(toml)
}

fn collect(
    file: &Path,
    triple: &str,
    build: &mut Vec<String>,
    target: &mut Vec<String>,
    cfgs: &mut Vec<String>,
    depth: usize,
) {
    let Ok(contents) = std::fs::read_to_string(file) else {
        return;
    };
    let Ok(table) = contents.parse::<toml::Table>() else {
        return;
    };
    let dir = file.parent().unwrap_or(Path::new("."));

    if depth < MAX_INCLUDE_DEPTH {
        for include in includes(&table, dir) {
            collect(&include, triple, build, target, cfgs, depth + 1);
        }
    }

    if let Some(flags) = table.get("build").and_then(|build| build.get("rustflags")) {
        build.extend(as_flags(flags));
    }

    let Some(entries) = table.get("target").and_then(toml::Value::as_table) else {
        return;
    };
    for (key, entry) in entries {
        if let Some(flags) = entry.get("rustflags") {
            if key == triple {
                target.extend(as_flags(flags));
            } else if key.starts_with("cfg(") {
                cfgs.push(key.clone());
            }
        }
    }
}

/// `include` entries, resolved relative to the including file.
fn includes(table: &toml::Table, dir: &Path) -> Vec<PathBuf> {
    let entries = match table.get("include") {
        Some(toml::Value::Array(entries)) => entries.iter().collect::<Vec<_>>(),
        Some(value @ toml::Value::Table(_)) => vec![value],
        _ => return Vec::new(),
    };

    entries
        .into_iter()
        .filter_map(|entry| {
            let (path, optional) = match entry {
                toml::Value::String(path) => (path.as_str(), false),
                toml::Value::Table(entry) => (
                    entry.get("path")?.as_str()?,
                    entry
                        .get("optional")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false),
                ),
                _ => return None,
            };
            // Cargo only accepts `.toml` paths in `include`.
            if !path.ends_with(".toml") {
                return None;
            }
            let path = dir.join(path);
            if optional && !path.is_file() {
                return None;
            }
            Some(path)
        })
        .collect()
}

/// A string is split on whitespace; each array element is a single flag.
fn as_flags(value: &toml::Value) -> Vec<String> {
    match value {
        toml::Value::String(flags) => flags.split_whitespace().map(str::to_owned).collect(),
        toml::Value::Array(flags) => flags
            .iter()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}
