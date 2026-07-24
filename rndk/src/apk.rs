use serde::Deserialize;

use crate::error::NdkError;
use crate::manifest::AndroidManifest;
use crate::ndk::{Key, Ndk};
use crate::target::Target;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, result::ZipError};
use zip::write::{ExtendedFileOptions, FileOptions};

/// Output format for the Android package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildFormat {
    Apk,
    Aab,
}

/// The options for how to treat debug symbols that are present in any `.so`
/// files that are added to the APK.
///
/// Using [`strip`](https://doc.rust-lang.org/cargo/reference/profiles.html#strip)
/// or [`split-debuginfo`](https://doc.rust-lang.org/cargo/reference/profiles.html#split-debuginfo)
/// in your cargo manifest(s) may cause debug symbols to not be present in a
/// `.so`, which would cause these options to do nothing.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StripConfig {
    /// Does not treat debug symbols specially
    #[default]
    Default,
    /// Removes debug symbols from the library before copying it into the APK
    Strip,
    /// Splits the library into into an ELF (`.so`) and DWARF (`.dwarf`). Only the
    /// `.so` is copied into the APK
    Split,
}

pub struct ApkConfig {
    pub ndk: Ndk,
    pub build_dir: PathBuf,
    pub apk_name: String,
    pub assets: Option<PathBuf>,
    pub resources: Option<PathBuf>,
    pub manifest: AndroidManifest,
    pub disable_aapt_compression: bool,
    pub strip: StripConfig,
    pub reverse_port_forward: HashMap<String, String>,
    pub format: BuildFormat,
    pub align: u32,
    pub normalize_zip: bool,
    pub zip_timestamp: Option<u64>,
}

impl ApkConfig {
    fn build_tool(&self, tool: &'static str) -> Result<Command, NdkError> {
        let mut cmd = self.ndk.build_tool(tool)?;
        cmd.current_dir(&self.build_dir);
        Ok(cmd)
    }

    fn unaligned_apk(&self) -> PathBuf {
        self.build_dir
            .join(format!("{}-unaligned.apk", self.apk_name))
    }

    /// Retrieves the path of the APK that will be written when [`UnsignedApk::sign`]
    /// is invoked
    #[inline]
    pub fn apk(&self) -> PathBuf {
        self.build_dir.join(format!("{}.apk", self.apk_name))
    }

    /// Retrieves the path of the AAB that will be written when [`UnsignedApk::sign`]
    /// is invoked
    #[inline]
    pub fn aab(&self) -> PathBuf {
        self.build_dir.join(format!("{}.aab", self.apk_name))
    }

    /// Retrieves the final output path (.apk or .aab) based on [`BuildFormat`]
    #[inline]
    pub fn output_path(&self) -> PathBuf {
        match self.format {
            BuildFormat::Apk => self.apk(),
            BuildFormat::Aab => self.aab(),
        }
    }

    pub fn create_apk(&self) -> Result<UnalignedApk<'_>, NdkError> {
        match self.format {
            BuildFormat::Apk => self.create_apk_impl(),
            BuildFormat::Aab => self.create_aab_impl(),
        }
    }

    fn create_apk_impl(&self) -> Result<UnalignedApk<'_>, NdkError> {
        std::fs::create_dir_all(&self.build_dir)?;
        self.manifest.write_to(&self.build_dir)?;

        let target_sdk_version = self
            .manifest
            .sdk
            .target_sdk_version
            .unwrap_or_else(|| self.ndk.default_target_platform());

        let mut aapt = self.build_tool(bin!("aapt"))?;
        aapt.arg("package")
            .arg("-f")
            .arg("-F")
            .arg(self.unaligned_apk())
            .arg("-M")
            .arg("AndroidManifest.xml")
            .arg("-I")
            .arg(self.ndk.android_jar(target_sdk_version)?);

        if self.disable_aapt_compression {
            aapt.arg("-0").arg("");
        }

        if let Some(res) = &self.resources {
            aapt.arg("-S").arg(res);
        }

        if let Some(assets) = &self.assets {
            aapt.arg("-A").arg(assets);
        }

        if !aapt.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(aapt)));
        }

        Ok(UnalignedApk {
            config: self,
            pending_entries: HashSet::default(),
        })
    }

    fn create_aab_impl(&self) -> Result<UnalignedApk<'_>, NdkError> {
        std::fs::create_dir_all(&self.build_dir)?;
        self.manifest.write_to(&self.build_dir)?;

        let target_sdk_version = self
            .manifest
            .sdk
            .target_sdk_version
            .unwrap_or_else(|| self.ndk.default_target_platform());

        let mut aapt2 = self.build_tool(bin!("aapt2"))?;
        aapt2.arg("link")
            .arg("--proto-format")
            .arg("-o")
            .arg(self.build_dir.join("base.apk"))
            .arg("-I")
            .arg(self.ndk.android_jar(target_sdk_version)?)
            .arg("--manifest")
            .arg(self.build_dir.join("AndroidManifest.xml"));

        if let Some(res) = &self.resources {
            let compiled = self.build_dir.join("compiled_res");
            fs::create_dir_all(&compiled)?;
            let mut compile = self.build_tool(bin!("aapt2"))?;
            compile.arg("compile").arg("-o").arg(&compiled).arg("--dir").arg(res);
            if !compile.status()?.success() {
                return Err(NdkError::CmdFailed(Box::new(compile)));
            }
            for entry in fs::read_dir(&compiled)? {
                let entry = entry?;
                if entry.path().extension().map_or(false, |e| e == "flat") {
                    aapt2.arg("-R").arg(entry.path());
                }
            }
        }

        if let Some(assets) = &self.assets {
            aapt2.arg("-A").arg(assets);
        }

        if !aapt2.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(aapt2)));
        }

        Ok(UnalignedApk {
            config: self,
            pending_entries: HashSet::default(),
        })
    }
}

pub struct UnalignedApk<'a> {
    config: &'a ApkConfig,
    pending_entries: HashSet<String>,
}

impl<'a> UnalignedApk<'a> {
    pub fn config(&self) -> &ApkConfig {
        self.config
    }

    pub fn add_lib(&mut self, path: &Path, target: Target) -> Result<(), NdkError> {
        if !path.exists() {
            return Err(NdkError::PathNotFound(path.into()));
        }
        let abi = target.android_abi();
        let lib_path = Path::new("lib").join(abi).join(path.file_name().unwrap());
        let out = self.config.build_dir.join(&lib_path);
        std::fs::create_dir_all(out.parent().unwrap())?;

        match self.config.strip {
            StripConfig::Default => {
                std::fs::copy(path, out)?;
            }
            StripConfig::Strip | StripConfig::Split => {
                let obj_copy = self.config.ndk.toolchain_bin("objcopy", target)?;

                {
                    let mut cmd = Command::new(&obj_copy);
                    cmd.arg("--strip-debug");
                    cmd.arg(path);
                    cmd.arg(&out);

                    if !cmd.status()?.success() {
                        return Err(NdkError::CmdFailed(Box::new(cmd)));
                    }
                }

                if self.config.strip == StripConfig::Split {
                    let dwarf_path = out.with_extension("dwarf");

                    {
                        let mut cmd = Command::new(&obj_copy);
                        cmd.arg("--only-keep-debug");
                        cmd.arg(path);
                        cmd.arg(&dwarf_path);

                        if !cmd.status()?.success() {
                            return Err(NdkError::CmdFailed(Box::new(cmd)));
                        }
                    }

                    let mut cmd = Command::new(obj_copy);
                    cmd.arg(format!("--add-gnu-debuglink={}", dwarf_path.display()));
                    cmd.arg(out);

                    if !cmd.status()?.success() {
                        return Err(NdkError::CmdFailed(Box::new(cmd)));
                    }
                }
            }
        }

        // Pass UNIX path separators to `aapt` on non-UNIX systems, ensuring the resulting separator
        // is compatible with the target device instead of the host platform.
        // Otherwise, it results in a runtime error when loading the NativeActivity `.so` library.
        let lib_path_unix = lib_path.to_str().unwrap().replace('\\', "/");

        let archive_path = if self.config.format == BuildFormat::Aab {
            format!("base/{lib_path_unix}")
        } else {
            lib_path_unix
        };
        self.pending_entries.insert(archive_path);

        Ok(())
    }

    pub fn add_runtime_libs(
        &mut self,
        path: &Path,
        target: Target,
        search_paths: &[&Path],
    ) -> Result<(), NdkError> {
        let abi_dir = path.join(target.android_abi());
        for entry in fs::read_dir(&abi_dir).map_err(|e| NdkError::IoPathError(abi_dir, e))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension() == Some(OsStr::new("so")) {
                self.add_lib_recursively(&path, target, search_paths)?;
            }
        }
        Ok(())
    }

    pub fn add_file(&mut self, src: &Path, dst: &Path) -> Result<(), NdkError> {
        if !src.exists() {
            return Err(NdkError::PathNotFound(src.into()));
        }
        let out = self.config.build_dir.join(dst);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, out)?;

        let dst_unix = dst.to_string_lossy().replace('\\', "/");
        let archive_path = if self.config.format == BuildFormat::Aab {
            format!("base/{dst_unix}")
        } else {
            dst_unix
        };
        self.pending_entries.insert(archive_path);

        Ok(())
    }

    pub fn add_pending_libs_and_align(self) -> Result<UnsignedApk<'a>, NdkError> {
        match self.config.format {
            BuildFormat::Apk => self.finalize_apk(),
            BuildFormat::Aab => self.finalize_aab(),
        }
    }

    fn dos_date_time(&self) -> DateTime {
        self.config.zip_timestamp.map_or_else(
            || DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0).expect("valid DOS datetime"),
            |ts| {
                let odt = time::OffsetDateTime::from_unix_timestamp(ts as i64)
                    .expect("timestamp out of range for OffsetDateTime");
                let pdt = time::PrimitiveDateTime::new(odt.date(), odt.time());
                DateTime::try_from(pdt).unwrap_or_default()
            },
        )
    }

    fn finalize_apk(self) -> Result<UnsignedApk<'a>, NdkError> {
        // add libs in stable order
        let mut aapt = self.config.build_tool(bin!("aapt"))?;
        aapt.arg("add");
        if self.config.disable_aapt_compression {
            aapt.arg("-0").arg("");
        }
        aapt.arg(self.config.unaligned_apk());
        let mut entries: Vec<_> = self.pending_entries.into_iter().collect();
        entries.sort();
        for path_unix in entries {
            aapt.arg(path_unix);
        }
        if !aapt.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(aapt)));
        }

        // normalize zip before zipalign so offsets remain stable
        if self.config.normalize_zip {
            super::zipnorm::normalize_zip_in_place(
                self.config.unaligned_apk(),
                self.config.zip_timestamp,
            )
            .map_err(|e| NdkError::IoPathError(self.config.unaligned_apk(), e))?;
        }

        let mut zipalign = self.config.build_tool(bin!("zipalign"))?;
        zipalign.arg("-f").arg("-v");

        // overridden with CARGO_RAPK_PAGE_SIZE_KB (allowed values per zipalign: 4, 16, 64).
        // Requires Build-Tools >= 35.0.0.
        let page_size_kb = std::env::var("CARGO_RAPK_PAGE_SIZE_KB")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(16);
        let bt_ver = self.config.ndk.build_tools_version();
        if bt_ver >= "35.0.0" {
            zipalign.arg("-P").arg(page_size_kb.to_string());
        } else {
            eprintln!(
                "zipalign -P requires Build-Tools >= 35.0.0 (found {}); continuing without -P",
                bt_ver
            );
        }

        zipalign
            .arg(self.config.align.to_string())
            .arg(self.config.unaligned_apk())
            .arg(self.config.apk());
        if !zipalign.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(zipalign)));
        }

        Ok(UnsignedApk(self.config))
    }

    fn finalize_aab(self) -> Result<UnsignedApk<'a>, NdkError> {
        let dos_time = self.dos_date_time();
        let file = fs::File::create(self.config.aab())?;
        let mut zip = ZipWriter::new(file);

        // BundleConfig.pb:
        //   bundletool { version: "1.15.0" }
        //   type: "REGULAR"
        //   compression: UNCOMPRESSED (1)
        const BUNDLE_CONFIG_PB: &[u8] = &[
            0x0A, 0x08, // field 1 (bundletool), length 8
            0x0A, 0x06, // sub-field 1 (version), length 6
            0x31, 0x2E, 0x31, 0x35, 0x2E, 0x30, // "1.15.0"
            0x12, 0x07, // field 2 (type), length 7
            0x52, 0x45, 0x47, 0x55, 0x4C, 0x41, 0x52, // "REGULAR"
            0x18, 0x01, // field 3 (compression) = UNCOMPRESSED (1)
        ];
        {
            let opts: FileOptions<'_, ExtendedFileOptions> = FileOptions::default()
                .compression_method(CompressionMethod::Stored)
                .last_modified_time(dos_time);
            zip.start_file("BundleConfig.pb", opts)
                .map_err(zip_to_io)?;
            zip.write_all(BUNDLE_CONFIG_PB)?;
        }

        // Extract base.apk (created by aapt2 link --proto-format) into base/ prefix
        let base_apk = self.config.build_dir.join("base.apk");
        if base_apk.exists() {
            let data = fs::read(&base_apk)?;
            let cursor = std::io::Cursor::new(data);
            let mut base_archive = ZipArchive::new(cursor)
                .map_err(zip_to_io)?;
            let mut names: Vec<String> = (0..base_archive.len())
                .filter_map(|i| base_archive.by_index(i).ok().map(|f| f.name().to_string()))
                .collect();
            names.sort();
            for name in names {
                let mut file = base_archive.by_name(&name)
                    .map_err(zip_to_io)?;
                let method = match file.compression() {
                    CompressionMethod::Stored => CompressionMethod::Stored,
                    _ => CompressionMethod::Deflated,
                };
                let opts: FileOptions<'_, ExtendedFileOptions> = FileOptions::default()
                    .compression_method(method)
                    .last_modified_time(dos_time);
                let mut buf = Vec::with_capacity(file.size() as usize);
                file.read_to_end(&mut buf)?;
                let aab_path = if name == "AndroidManifest.xml" {
                    format!("base/manifest/{name}")
                } else {
                    format!("base/{name}")
                };
                zip.start_file(&aab_path, opts)
                    .map_err(zip_to_io)?;
                zip.write_all(&buf)?;
            }
        }

        // Add pending entries (libs, dex) with stored compression for .so/.dex
        let mut entries: Vec<_> = self.pending_entries.into_iter().collect();
        entries.sort();
        for entry in entries {
            let rel_src = entry
                .strip_prefix("base/")
                .unwrap_or(&entry)
                .to_string();
            let src = self.config.build_dir.join(&rel_src);
            let ext = Path::new(&entry)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let method = match ext {
                "so" | "dex" => CompressionMethod::Stored,
                _ => CompressionMethod::Deflated,
            };
            let opts: FileOptions<'_, ExtendedFileOptions> = FileOptions::default()
                .compression_method(method)
                .last_modified_time(dos_time);
            // Map entries to correct AAB paths
            let aab_entry = if entry == "base/classes.dex" {
                "base/dex/classes.dex".to_string()
            } else {
                entry.clone()
            };
            zip.start_file(&aab_entry, opts)
                .map_err(zip_to_io)?;
            let mut f = fs::File::open(&src)?;
            std::io::copy(&mut f, &mut zip)?;
        }

        zip.finish()
            .map_err(zip_to_io)?;

        // Normalize AAB zip if requested
        if self.config.normalize_zip {
            super::zipnorm::normalize_zip_in_place(
                self.config.aab(),
                self.config.zip_timestamp,
            )
            .map_err(|e| NdkError::IoPathError(self.config.aab(), e))?;
        }

        Ok(UnsignedApk(self.config))
    }
}

pub struct UnsignedApk<'a>(&'a ApkConfig);
impl<'a> UnsignedApk<'a> {
    pub fn config(&self) -> &'a ApkConfig {
        self.0
    }

    pub fn sign(self, key: Key) -> Result<Apk, NdkError> {
        let mut apksigner = self.0.build_tool(bat!("apksigner"))?;

        apksigner.env("CARGO_RAPK_KS_PASS", &key.password);
        apksigner
            .arg("sign")
            .arg("--ks")
            .arg(&key.path)
            .arg("--ks-pass")
            .arg("env:CARGO_RAPK_KS_PASS");

        if self.0.normalize_zip {
            apksigner
                .arg("--v1-signing-enabled")
                .arg("false")
                .arg("--v2-signing-enabled")
                .arg("true")
                .arg("--v3-signing-enabled")
                .arg("true")
                .arg("--v4-signing-enabled")
                .arg("false");
        }

        apksigner.arg(self.0.output_path());

        if !apksigner.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(apksigner)));
        }
        Ok(Apk::from_config(self.0))
    }
}

fn zip_to_io(e: ZipError) -> std::io::Error {
    match e {
        ZipError::Io(ioe) => ioe,
        other => std::io::Error::new(std::io::ErrorKind::Other, other.to_string()),
    }
}

pub struct Apk {
    path: PathBuf,
    package_name: String,
    activity_name: String,
    ndk: Ndk,
    reverse_port_forward: HashMap<String, String>,
}

impl Apk {
    pub fn from_config(config: &ApkConfig) -> Self {
        let ndk = config.ndk.clone();
        let activity_name = config
            .manifest
            .application
            .activity
            .first()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "android.app.NativeActivity".to_string());
        Self {
            path: config.output_path(),
            package_name: config.manifest.package.clone(),
            activity_name,
            ndk,
            reverse_port_forward: config.reverse_port_forward.clone(),
        }
    }

    pub fn reverse_port_forwarding(&self, device_serial: Option<&str>) -> Result<(), NdkError> {
        for (from, to) in &self.reverse_port_forward {
            println!("Reverse port forwarding from {from} to {to}");
            let mut adb = self.ndk.adb(device_serial)?;

            adb.arg("reverse").arg(from).arg(to);

            if !adb.status()?.success() {
                return Err(NdkError::CmdFailed(Box::new(adb)));
            }
        }

        Ok(())
    }

    pub fn install(&self, device_serial: Option<&str>) -> Result<(), NdkError> {
        let mut adb = self.ndk.adb(device_serial)?;

        adb.arg("install").arg("-r").arg(&self.path);
        if !adb.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(adb)));
        }
        Ok(())
    }

    pub fn start(&self, device_serial: Option<&str>) -> Result<(), NdkError> {
        let mut adb = self.ndk.adb(device_serial)?;
        adb.arg("shell")
            .arg("am")
            .arg("start")
            .arg("-a")
            .arg("android.intent.action.MAIN")
            .arg("-c")
            .arg("android.intent.category.LAUNCHER")
            .arg("-n")
            .arg(format!("{}/{}", self.package_name, self.activity_name));

        if !adb.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(adb)));
        }

        Ok(())
    }

    pub fn uidof(&self, device_serial: Option<&str>) -> Result<u32, NdkError> {
        let mut adb = self.ndk.adb(device_serial)?;
        adb.arg("shell")
            .arg("pm")
            .arg("list")
            .arg("package")
            .arg("-U")
            .arg(&self.package_name);
        let output = adb.output()?;

        if !output.status.success() {
            return Err(NdkError::CmdFailed(Box::new(adb)));
        }

        let output = std::str::from_utf8(&output.stdout).unwrap();
        let (_package, uid) = output
            .lines()
            .filter_map(|line| line.split_once(' '))
            // `pm list package` uses the id as a substring filter; make sure
            // we select the right package in case it returns multiple matches:
            .find(|(package, _uid)| package.strip_prefix("package:") == Some(&self.package_name))
            .ok_or(NdkError::PackageNotInOutput {
                package: self.package_name.clone(),
                output: output.to_owned(),
            })?;
        let uid = uid
            .strip_prefix("uid:")
            .ok_or(NdkError::UidNotInOutput(output.to_owned()))?;
        let uid = uid
            .split(',')
            .find(|part| !part.trim().is_empty())
            .unwrap_or(uid)
            .trim();
        uid.parse()
            .map_err(|e| NdkError::NotAUid(e, uid.to_owned()))
    }
}
