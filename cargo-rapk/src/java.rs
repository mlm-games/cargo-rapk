use crate::error::Error;
use rndk::error::NdkError;
use rndk::ndk::Ndk;
use std::fs;
use std::path::{Path, PathBuf};

fn collect_kotlin_files(source_dirs: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
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

fn collect_java_files(source_dirs: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
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

fn collect_jar_files(source_dirs: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
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

pub(crate) fn compile_java_sources(
    ndk: &Ndk,
    source_dirs: &[PathBuf],
    build_dir: &Path,
    min_sdk_version: u32,
    target_sdk_version: u32,
) -> Result<Vec<PathBuf>, Error> {
    let java_files = collect_java_files(source_dirs)?;
    let kt_files = collect_kotlin_files(source_dirs)?;
    let jar_files = collect_jar_files(source_dirs)?;
    if java_files.is_empty() && kt_files.is_empty() && jar_files.is_empty() {
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
    for jar_file in &jar_files {
        classpath.push(path_separator);
        classpath.push_str(&jar_file.to_string_lossy());
    }

    // If Kotlin sources are present, add kotlin-stdlib from the compiler distribution.
    let kotlin_stdlib_jar = if !kt_files.is_empty() {
        let j = ndk.kotlin_stdlib_jar()?;
        classpath.push(path_separator);
        classpath.push_str(&j.to_string_lossy());
        Some(j)
    } else {
        None
    };

    // Compile sources
    if !java_files.is_empty() {
        let mut javac = ndk.javac()?;
        javac
            .arg("-encoding")
            .arg("UTF-8")
            .arg("-source")
            .arg("8")
            .arg("-target")
            .arg("8")
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
        kotlinc
            .arg("-classpath")
            .arg(&classpath)
            .arg("-d")
            .arg(&classes_dir);
        for kt_file in &kt_files {
            kotlinc.arg(kt_file);
        }
        if !kotlinc.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(kotlinc)).into());
        }
    }

    let class_files = collect_class_files(&classes_dir)?;

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
