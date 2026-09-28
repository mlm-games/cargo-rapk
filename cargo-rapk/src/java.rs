use crate::error::Error;
use rndk::error::NdkError;
use rndk::ndk::Ndk;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn collect_kotlin_files(source_dirs: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let mut kt_files = Vec::new();
    for source_dir in source_dirs {
        if !source_dir.exists() {
            return Err(NdkError::PathNotFound(source_dir.clone()).into());
        }
        if !source_dir.is_dir() {
            return Err(NdkError::PathNotFound(source_dir.clone()).into());
        }

        let mut stack = vec![source_dir.clone()];
        while let Some(current_dir) = stack.pop() {
            for entry in fs::read_dir(&current_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("kt") {
                    kt_files.push(path);
                }
            }
        }
    }

    kt_files.sort();
    Ok(kt_files)
}

pub(crate) fn collect_java_files(source_dirs: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let mut java_files = Vec::new();
    for source_dir in source_dirs {
        if !source_dir.exists() {
            return Err(NdkError::PathNotFound(source_dir.clone()).into());
        }
        if !source_dir.is_dir() {
            return Err(NdkError::PathNotFound(source_dir.clone()).into());
        }

        let mut stack = vec![source_dir.clone()];
        while let Some(current_dir) = stack.pop() {
            for entry in fs::read_dir(&current_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("java") {
                    java_files.push(path);
                }
            }
        }
    }

    java_files.sort();
    Ok(java_files)
}

pub(crate) fn collect_jar_files(source_dirs: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let mut jar_files = Vec::new();
    for source_dir in source_dirs {
        let mut stack = vec![source_dir.clone()];
        while let Some(current_dir) = stack.pop() {
            for entry in fs::read_dir(&current_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("jar") {
                    jar_files.push(path);
                }
            }
        }
    }

    jar_files.sort();
    Ok(jar_files)
}

fn collect_class_files(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut class_files = Vec::new();
    if !dir.exists() {
        return Ok(class_files);
    }

    let mut stack = vec![dir.to_path_buf()];
    while let Some(current_dir) = stack.pop() {
        for entry in fs::read_dir(&current_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("class") {
                class_files.push(path);
            }
        }
    }

    class_files.sort();
    Ok(class_files)
}

/// Compiles generated `R.java` sources, returning the class files so they can
/// be dexed alongside the rest.
pub(crate) fn compile_r_java(
    ndk: &Ndk,
    r_sources: &[PathBuf],
    classpath_jars: &[PathBuf],
    out_dir: &Path,
    build_dir: &Path,
) -> Result<Vec<PathBuf>, Error> {
    if r_sources.is_empty() {
        return Ok(Vec::new());
    }
    if out_dir.exists() {
        fs::remove_dir_all(out_dir)?;
    }
    fs::create_dir_all(out_dir)?;
    let _ = build_dir;

    let path_separator = if cfg!(target_os = "windows") {
        ';'
    } else {
        ':'
    };
    let mut classpath = ndk
        .android_jar(ndk.default_target_platform())?
        .to_string_lossy()
        .into_owned();
    for jar in classpath_jars {
        classpath.push(path_separator);
        classpath.push_str(&jar.to_string_lossy());
    }

    let mut javac = ndk.javac()?;
    javac
        .arg("-nowarn")
        .arg("-encoding")
        .arg("UTF-8")
        .arg("--release")
        .arg("8")
        .arg("-proc:none")
        .arg("-classpath")
        .arg(&classpath)
        .arg("-d")
        .arg(out_dir);
    for source in r_sources {
        javac.arg(source);
    }
    if !javac.status()?.success() {
        return Err(NdkError::CmdFailed(Box::new(javac)).into());
    }
    collect_class_files(out_dir)
}

/// Everything the Java/Kotlin/dex stage needs beyond the source directories.
pub(crate) struct DexInputs<'a> {
    /// Jars from `android_libs`, on the classpath and dexed.
    pub lib_jars: &'a [PathBuf],
    /// Pre-compiled classes (generated `R` classes), on the classpath and dexed.
    pub classes: &'a [PathBuf],
    /// Directories added to the classpath only, e.g. the generated `R` output.
    pub classpath: &'a [PathBuf],
}

pub(crate) fn compile_java_sources(
    ndk: &Ndk,
    source_dirs: &[PathBuf],
    inputs: DexInputs<'_>,
    build_dir: &Path,
    min_sdk_version: u32,
    target_sdk_version: u32,
) -> Result<Vec<PathBuf>, Error> {
    let DexInputs {
        lib_jars,
        classes: extra_classes,
        classpath: extra_classpath,
    } = inputs;
    let java_files = collect_java_files(source_dirs)?;
    let kt_files = collect_kotlin_files(source_dirs)?;
    let mut jar_files = collect_jar_files(source_dirs)?;
    for lib_jar in lib_jars {
        if !jar_files.contains(lib_jar) {
            jar_files.push(lib_jar.clone());
        }
    }
    jar_files.sort();
    if java_files.is_empty()
        && kt_files.is_empty()
        && jar_files.is_empty()
        && extra_classes.is_empty()
    {
        return Ok(Vec::new());
    }

    let java_build_dir = build_dir.join("java");
    let classes_dir = java_build_dir.join("classes");
    let dex_dir = java_build_dir.join("dex");
    if java_build_dir.exists() {
        fs::remove_dir_all(&java_build_dir)?;
    }
    fs::create_dir_all(&classes_dir)?;
    fs::create_dir_all(&dex_dir)?;

    let android_jar = ndk.android_jar(target_sdk_version)?;
    let path_separator = if cfg!(target_os = "windows") {
        ';'
    } else {
        ':'
    };
    let mut classpath = android_jar.to_string_lossy().into_owned();
    for entry in jar_files.iter().chain(extra_classpath) {
        classpath.push(path_separator);
        classpath.push_str(&entry.to_string_lossy());
    }

    // `-no-stdlib` is opt-in
    let kotlin_stdlib_jar = if !kt_files.is_empty() {
        Some(ndk.kotlin_stdlib_jar()?)
    } else {
        None
    };

    // Compile sources
    if !java_files.is_empty() {
        let mut javac = ndk.javac()?;
        javac
            .arg("-encoding")
            .arg("UTF-8")
            .arg("--release")
            .arg("8")
            // A jar on the classpath that ships a
            // `META-INF/services/javax.annotation.processing.Processor` would
            // otherwise be executed during compilation. Nothing here needs
            // annotation processing.
            .arg("-proc:none")
            .arg("-classpath")
            .arg(&classpath)
            .arg("-d")
            .arg(&classes_dir);
        for java_file in &java_files {
            javac.arg(java_file);
        }
        if !javac.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(javac)).into());
        }
    }

    if !kt_files.is_empty() {
        let mut kotlinc = ndk.kotlinc()?;
        // -no-stdlib + explicit stdlib first has identical output to the dist
        // script's auto-added stdlib, but works for both the launcher script
        // and the `java -cp … K2JVMCompiler` Maven mode.
        let mut kc_classpath = kotlin_stdlib_jar
            .as_ref()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        kc_classpath.push(path_separator);
        kc_classpath.push_str(&classpath);
        kotlinc
            // Pinned so output doesn't follow kotlinc's default (already 1.8, made explicit).
            .arg("-jvm-target")
            .arg("1.8")
            .arg("-no-stdlib")
            .arg("-classpath")
            .arg(&kc_classpath)
            .arg("-d")
            .arg(&classes_dir);
        for kt_file in &kt_files {
            kotlinc.arg(kt_file);
        }
        if !kotlinc.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(kotlinc)).into());
        }
    }

    let mut class_files = collect_class_files(&classes_dir)?;
    for extra in extra_classes {
        class_files.push(extra.clone());
    }
    class_files.sort();
    class_files.dedup();

    // Checked before d8 runs, so a conflict is a diagnosis naming the two
    // artifacts rather than `d8`'s own report, which quotes the whole classpath
    // and leaves the reader to find the pair inside it. Everything handed to d8
    // has to be here: a Kotlin app that also lists `kotlin-stdlib` in
    // `android_libs` gets the toolchain's own stdlib added to the dex, and that
    // pair is a duplicate the check would otherwise miss.
    let mut dexed: Vec<PathBuf> = jar_files.clone();
    dexed.extend(kotlin_stdlib_jar.iter().cloned());
    let duplicates = rndk::maven::duplicate_classes(&dexed).unwrap_or_default();
    if !duplicates.is_empty() {
        let mut report = String::new();
        for (class, owners) in duplicates.iter().take(20) {
            report.push_str(&format!("\n  {class}\n    {}", owners.join("\n    ")));
        }
        if duplicates.len() > 20 {
            report.push_str(&format!("\n  ... and {} more", duplicates.len() - 20));
        }
        return Err(NdkError::DuplicateClasses {
            count: duplicates.len(),
            classes: report,
        }
        .into());
    }

    let mut d8 = ndk.d8()?;
    d8.arg("--lib")
        .arg(&android_jar)
        .arg("--min-api")
        .arg(min_sdk_version.to_string())
        .arg("--output")
        .arg(&dex_dir);
    if !class_files.is_empty() {
        for class_file in &class_files {
            d8.arg(class_file);
        }
    }
    for jar_file in &jar_files {
        d8.arg(jar_file);
    }
    if let Some(ref stdlib_jar) = kotlin_stdlib_jar {
        d8.arg(stdlib_jar);
    }
    if !d8.status()?.success() {
        return Err(NdkError::CmdFailed(Box::new(d8)).into());
    }

    let mut dex_files = Vec::new();
    for entry in fs::read_dir(&dex_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("dex") {
            dex_files.push(path);
        }
    }
    dex_files.sort();

    if dex_files.is_empty() {
        return Err(NdkError::PathNotFound(dex_dir.join("classes.dex")).into());
    }

    Ok(dex_files)
}
