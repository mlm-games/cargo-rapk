use serde::Deserialize;

use crate::error::NdkError;
use crate::libs::LibResolver;
use crate::manifest::AndroidManifest;
use crate::ndk::{Key, Ndk};
use crate::target::Target;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::write::{ExtendedFileOptions, FileOptions};
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, result::ZipError};

/// Output format for the Android package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildFormat {
    Apk,
    Aab,
}

/// Android's `PER_USER_RANGE`: app ids are offset by this much per user, so
/// `uid / PER_USER_RANGE` is the user id.
const PER_USER_RANGE: u32 = 100_000;

/// `fs::copy` also copies permission bits, which makes a rebuild fail when the
/// source is read-only (a read-only SDK in `/nix/store`, for example).
fn set_readable(path: &Path) -> Result<(), NdkError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Archive entry names are always `/`-separated, whatever the host uses.
fn to_unix_separators(path: &Path) -> Result<String, NdkError> {
    path.to_str()
        .map(|path| path.replace('\\', "/"))
        .ok_or_else(|| NdkError::NonUtf8Path(path.into()))
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

        let mut aapt = self.build_tool("aapt")?;
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

        let mut aapt2 = self.build_tool("aapt2")?;
        aapt2
            .arg("link")
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
            let mut compile = self.build_tool("aapt2")?;
            compile
                .arg("compile")
                .arg("-o")
                .arg(&compiled)
                .arg("--dir")
                .arg(res);
            if !compile.status()?.success() {
                return Err(NdkError::CmdFailed(Box::new(compile)));
            }
            for entry in fs::read_dir(&compiled)? {
                let entry = entry?;
                if entry.path().extension().is_some_and(|e| e == "flat") {
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
        let name = path
            .file_name()
            .ok_or_else(|| NdkError::PathNotFound(path.into()))?;
        let lib_path = Path::new("lib").join(abi).join(name);
        let out = self.config.build_dir.join(&lib_path);
        let out_parent = out
            .parent()
            .ok_or_else(|| NdkError::PathNotFound(out.clone()))?;
        std::fs::create_dir_all(out_parent)?;

        match self.config.strip {
            StripConfig::Default => {
                std::fs::copy(path, &out)?;
                set_readable(&out)?;
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
        let lib_path_unix = to_unix_separators(&lib_path)?;

        let archive_path = if self.config.format == BuildFormat::Aab {
            format!("base/{lib_path_unix}")
        } else {
            lib_path_unix
        };
        self.pending_entries.insert(archive_path);

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

        let dst_unix = to_unix_separators(dst)?;
        let archive_path = if self.config.format == BuildFormat::Aab {
            format!("base/{dst_unix}")
        } else {
            dst_unix
        };
        self.pending_entries.insert(archive_path);

        Ok(())
    }

    /// Packages the resolved library closure. Each name is added exactly once,
    /// so the result does not depend on the order the closure was discovered in.
    pub fn add_libs(&mut self, libs: &mut LibResolver, target: Target) -> Result<(), NdkError> {
        for path in libs.resolve()?.into_values() {
            self.add_lib(&path, target)?;
        }
        Ok(())
    }

    pub fn add_pending_libs_and_align(self) -> Result<UnsignedApk<'a>, NdkError> {
        match self.config.format {
            BuildFormat::Apk => self.finalize_apk(),
            BuildFormat::Aab => self.finalize_aab(),
        }
    }

    fn dos_date_time(&self) -> Result<DateTime, NdkError> {
        match self.config.zip_timestamp {
            None => crate::zipnorm::dos_epoch(),
            Some(ts) => crate::zipnorm::unix_ts_to_dos(ts),
        }
    }

    fn finalize_apk(self) -> Result<UnsignedApk<'a>, NdkError> {
        // add libs in stable order
        let mut aapt = self.config.build_tool("aapt")?;
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
            )?;
        }

        let mut zipalign = self.config.build_tool("zipalign")?;
        zipalign.arg("-f").arg("-v");

        // overridden with CARGO_RAPK_PAGE_SIZE_KB (allowed values per zipalign: 4, 16, 64).
        // Requires Build-Tools >= 35.0.0.
        let page_size_kb = std::env::var("CARGO_RAPK_PAGE_SIZE_KB")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(16);
        let bt_ver = self.config.ndk.build_tools_version();
        if self.config.ndk.build_tools_at_least((35, 0, 0)) {
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
        let dos_time = self.dos_date_time()?;
        let file = fs::File::create(self.config.aab())?;
        let mut zip = ZipWriter::new(file);

        // BundleConfig.pb (android.bundle.BundleConfig proto):
        //   bundletool { version: "1.15.0" }
        // `type` is field 8 (REGULAR = 0, the default) so it is omitted.
        // NOTE: field 2 is `optimizations` and field 3 is `compression`
        // (both messages) -- do NOT emit raw strings/enums there; bundletool
        // fails to parse the bundle otherwise.
        const BUNDLE_CONFIG_PB: &[u8] = &[
            0x0A, 0x08, // field 1 (bundletool), length 8
            0x12, 0x06, // sub-field 2 (version), length 6
            0x31, 0x2E, 0x31, 0x35, 0x2E, 0x30, // "1.15.0"
        ];
        {
            let opts: FileOptions<'_, ExtendedFileOptions> = FileOptions::default()
                .compression_method(CompressionMethod::Stored)
                .last_modified_time(dos_time);
            zip.start_file("BundleConfig.pb", opts).map_err(zip_to_io)?;
            zip.write_all(BUNDLE_CONFIG_PB)?;
        }

        // Extract base.apk (created by aapt2 link --proto-format) into base/ prefix
        let base_apk = self.config.build_dir.join("base.apk");
        if base_apk.exists() {
            let data = fs::read(&base_apk)?;
            let cursor = std::io::Cursor::new(data);
            let mut base_archive = ZipArchive::new(cursor).map_err(zip_to_io)?;
            let mut names: Vec<String> = (0..base_archive.len())
                .filter_map(|i| base_archive.by_index(i).ok().map(|f| f.name().to_string()))
                .collect();
            names.sort();
            for name in names {
                let mut file = base_archive.by_name(&name).map_err(zip_to_io)?;
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
                zip.start_file(&aab_path, opts).map_err(zip_to_io)?;
                zip.write_all(&buf)?;
            }
        }

        // Add pending entries (libs, dex) with stored compression for .so/.dex
        let mut entries: Vec<_> = self.pending_entries.into_iter().collect();
        entries.sort();
        for entry in entries {
            let rel_src = entry.strip_prefix("base/").unwrap_or(&entry).to_string();
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
            zip.start_file(&aab_entry, opts).map_err(zip_to_io)?;
            let mut f = fs::File::open(&src)?;
            std::io::copy(&mut f, &mut zip)?;
        }

        zip.finish().map_err(zip_to_io)?;

        // Normalize AAB zip if requested
        if self.config.normalize_zip {
            super::zipnorm::normalize_zip_in_place(self.config.aab(), self.config.zip_timestamp)?;
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
        // AABs are plain JARs: Google requires `jarsigner`, and `apksigner`
        // refuses them ("Missing AndroidManifest.xml"). APKs keep apksigner.
        if self.0.format == BuildFormat::Aab {
            return self.sign_aab(key);
        }

        let mut apksigner = self.0.ndk.apksigner()?;
        apksigner.current_dir(&self.0.build_dir);

        apksigner.env("CARGO_RAPK_KS_PASS", &key.password);
        apksigner
            .arg("sign")
            .arg("--ks")
            .arg(&key.path)
            .arg("--ks-pass")
            .arg("env:CARGO_RAPK_KS_PASS");
        if let Some(alias) = key.alias.as_deref().filter(|a| !a.is_empty()) {
            apksigner.arg("--ks-key-alias").arg(alias);
        }
        if let Some(key_pass) = key.key_password.as_deref().filter(|p| !p.is_empty()) {
            apksigner.env("CARGO_RAPK_KEY_PASS", key_pass);
            apksigner.arg("--key-pass").arg("env:CARGO_RAPK_KEY_PASS");
        }

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

    /// Sign an AAB with `jarsigner` (JAR signing scheme).
    fn sign_aab(self, key: Key) -> Result<Apk, NdkError> {
        let alias = key
            .alias
            .as_deref()
            .filter(|a| !a.is_empty())
            .ok_or(NdkError::MissingKeyAlias(self.0.apk_name.clone()));
        let mut jarsigner = self.0.ndk.jarsigner()?;
        jarsigner.env("CARGO_RAPK_KS_PASS", &key.password);
        jarsigner
            .arg("-keystore")
            .arg(&key.path)
            .arg("-storepass:env")
            .arg("CARGO_RAPK_KS_PASS");
        if let Some(key_pass) = key.key_password.as_deref().filter(|p| !p.is_empty()) {
            jarsigner.env("CARGO_RAPK_KEY_PASS", key_pass);
            jarsigner.arg("-keypass:env").arg("CARGO_RAPK_KEY_PASS");
        }
        jarsigner.arg(self.0.output_path()).arg(alias?);

        if !jarsigner.status()?.success() {
            return Err(NdkError::CmdFailed(Box::new(jarsigner)));
        }
        Ok(Apk::from_config(self.0))
    }
}

fn zip_to_io(e: ZipError) -> std::io::Error {
    match e {
        ZipError::Io(ioe) => ioe,
        other => std::io::Error::other(other.to_string()),
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

        // `adb install` has no `--user`; scoping the install would mean pushing
        // the APK and handing it to `pm install`, losing incremental install.
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
            // `current` is the documented spelling for the foreground user, which
            // is the one `cargo rapk` launched into and filters `logcat` by.
            .arg("--user")
            .arg("current")
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

    /// The foreground Android user, which on a multi-user device (one with a
    /// work profile, for instance) is not necessarily the only one the package
    /// is installed for. `pm list package -U` reports a uid per user, so this
    /// is what picks the one to filter `logcat` by.
    pub fn current_user(&self, device_serial: Option<&str>) -> Result<u32, NdkError> {
        let mut adb = self.ndk.adb(device_serial)?;
        adb.arg("shell").arg("am").arg("get-current-user");
        let output = adb.output()?;

        if !output.status.success() {
            return Err(NdkError::CmdFailed(Box::new(adb)));
        }

        let user = std::str::from_utf8(&output.stdout)
            .map_err(|_| NdkError::NonUtf8Output("adb shell am get-current-user"))?
            .trim();
        user.parse()
            .map_err(|e| NdkError::NotAUserId(e, user.to_owned()))
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

        let output = std::str::from_utf8(&output.stdout)
            .map_err(|_| NdkError::NonUtf8Output("adb shell pm list package"))?;
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
        let uids = uid
            .strip_prefix("uid:")
            .ok_or(NdkError::UidNotInOutput(output.to_owned()))?;

        // A multi-user device reports one uid per user, encoded as
        // `user_id * PER_USER_RANGE + app_id`; prefer the current user's.
        let user = self.current_user(device_serial)?;
        let uids = uids
            .split(',')
            .map(str::trim)
            .filter(|uid| !uid.is_empty())
            .collect::<Vec<_>>();
        let uid = uids
            .iter()
            .find(|uid| {
                uid.parse::<u32>()
                    .is_ok_and(|uid| uid / PER_USER_RANGE == user)
            })
            .or_else(|| uids.first())
            .ok_or(NdkError::UidNotInOutput(output.to_owned()))?;

        uid.parse()
            .map_err(|e| NdkError::NotAUid(e, (*uid).to_owned()))
    }
}
