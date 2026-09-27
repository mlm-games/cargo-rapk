use crate::contrib::collect_android_contributions;
use crate::error::Error;
use crate::java::{
    DexInputs, collect_jar_files, collect_java_files, collect_kotlin_files, compile_java_sources,
    compile_r_java,
};
use crate::manifest::{Inheritable, Manifest, Root};
use cargo_subcommand::{Artifact, ArtifactType, CrateType, Profile, Subcommand};
use rndk::apk::{Apk, ApkConfig, BuildFormat};
use rndk::cargo::{VersionCode, cargo_ndk};
use rndk::error::NdkError;
use rndk::libs::{LibResolver, LibSource};
use rndk::manifest::{IntentFilter, MetaData};
use rndk::ndk::{Key, Ndk};
use rndk::target::Target;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const NATIVE_ACTIVITY_NAME: &str = "android.app.NativeActivity";

#[derive(Clone, Copy)]
struct ReproCfg {
    deterministic: bool,
    unsigned: bool,
    align: u32,
    ts_unix: Option<u64>,
    no_normalize_zip: bool,
}

// Ensure a sane default alignment for zipalign.
impl Default for ReproCfg {
    fn default() -> Self {
        Self {
            deterministic: false,
            unsigned: false,
            align: 16,
            ts_unix: None,
            no_normalize_zip: false,
        }
    }
}

const UNIVERSAL_TARGETS: &[Target] = &[
    Target::Arm64V8a,
    Target::ArmV7a,
    Target::X86,
    Target::X86_64,
];

pub struct ApkBuilder<'a> {
    cmd: &'a Subcommand,
    ndk: Ndk,
    manifest: Manifest,
    base_version_code: u32,
    build_dir: PathBuf,
    build_targets: Vec<Target>,
    device_serial: Option<String>,
    repro: ReproCfg,
    format: BuildFormat,
    universal: bool,
}

impl<'a> ApkBuilder<'a> {
    pub fn from_subcommand(
        cmd: &'a Subcommand,
        device_serial: Option<String>,
    ) -> Result<Self, Error> {
        println!(
            "Using package `{}` in `{}`",
            cmd.package(),
            cmd.manifest().display()
        );
        let ndk = Ndk::from_env()?;
        let mut manifest = Manifest::parse_from_toml(cmd.manifest())?;
        let workspace_manifest: Option<Root> = cmd
            .workspace_manifest()
            .map(Root::parse_from_toml)
            .transpose()?;
        let build_targets = if let Some(target) = cmd.target() {
            vec![Target::from_rust_triple(target)?]
        } else if !manifest.build_targets.is_empty() {
            manifest.build_targets.clone()
        } else {
            vec![
                ndk.detect_abi(device_serial.as_deref())
                    .unwrap_or(Target::Arm64V8a),
            ]
        };
        let build_dir = dunce::simplified(cmd.target_dir())
            .join(cmd.profile())
            .join("apk");

        let package_version = match &manifest.version {
            Inheritable::Value(v) => v.clone(),
            Inheritable::Inherited { workspace: true } => {
                let workspace = workspace_manifest
                    .ok_or(Error::InheritanceMissingWorkspace)?
                    .workspace
                    .ok_or(Error::MissingWorkspaceTable)?;
                workspace
                    .package
                    .ok_or(Error::WorkspaceMissingInheritedField("package"))?
                    .version
                    .ok_or(Error::WorkspaceMissingInheritedField("package.version"))?
            }
            Inheritable::Inherited { workspace: false } => return Err(Error::InheritedFalse),
        };
        let version_code = manifest
            .android_manifest
            .sdk
            .version_code
            .unwrap_or(VersionCode::from_semver(&package_version)?.to_code(1));

        if manifest
            .android_manifest
            .version_name
            .replace(package_version)
            .is_some()
        {
            return Err(Error::VersionNameSet);
        }
        if manifest.android_manifest.version_code.is_some() {
            return Err(Error::VersionCodeSet);
        }

        manifest
            .android_manifest
            .sdk
            .target_sdk_version
            .get_or_insert_with(|| ndk.default_target_platform());

        manifest
            .android_manifest
            .application
            .debuggable
            .get_or_insert_with(|| *cmd.profile() == Profile::Dev);

        Ok(Self {
            cmd,
            ndk,
            manifest,
            base_version_code: version_code,
            build_dir,
            build_targets,
            device_serial,
            repro: ReproCfg::default(),
            format: BuildFormat::Apk,
            universal: false,
        })
    }

    pub fn set_format(&mut self, format: BuildFormat) {
        self.format = format;
    }

    /// Build for all four Android ABIs in a single artifact.
    /// Overrides any target set via `--target` or manifest `build-targets`.
    /// When set, each ABI gets its own `--target` in the cargo invocation
    /// regardless of any `--target` passed on the CLI.
    pub fn set_universal(&mut self) {
        self.build_targets = UNIVERSAL_TARGETS.to_vec();
        self.universal = true;
    }

    pub fn set_repro_flags(
        &mut self,
        deterministic: bool,
        unsigned: bool,
        align: u32,
        ts: Option<u64>,
        no_norm: bool,
    ) {
        let env_ts = std::env::var("SOURCE_DATE_EPOCH")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());

        self.repro = ReproCfg {
            deterministic,
            unsigned,
            align: if align == 0 { 4 } else { align },
            ts_unix: ts.or(env_ts),
            no_normalize_zip: no_norm,
        };
    }

    /// Forwards the parsed cargo args, dropping `--target` when `--universal`
    /// already substituted it, so cargo never sees two.
    fn apply_cargo_args(&self, cargo: &mut std::process::Command) {
        let mut args = self.cmd.args().clone();
        if self.universal {
            args.target = None;
        }
        args.apply(cargo);
    }

    /// The directory relative paths in the manifest resolve against.
    fn crate_path(&self) -> Result<&Path, Error> {
        self.cmd
            .manifest()
            .parent()
            .ok_or_else(|| Error::MissingManifestParent(self.cmd.manifest().to_path_buf()))
    }

    fn java_sources(&self) -> Result<Vec<PathBuf>, Error> {
        let crate_path = self.crate_path()?;
        let mut java_sources = self
            .manifest
            .java_sources
            .iter()
            .map(|p| dunce::simplified(&crate_path.join(p)).to_owned())
            .collect::<Vec<_>>();
        java_sources.extend(collect_android_contributions(self.cmd.manifest())?.java_sources);
        Ok(java_sources)
    }

    fn android_libs(&self) -> Result<Vec<String>, Error> {
        let mut libs = self.manifest.android_libs.clone();
        libs.extend(collect_android_contributions(self.cmd.manifest())?.android_libs);
        libs.dedup();
        Ok(libs)
    }

    /// Resolves every external tool and input the build needs, without
    /// compiling, dexing or packaging anything.
    fn preflight(&self) -> Result<(), Error> {
        let mut problems = Vec::new();

        let target_sdk_version = self
            .manifest
            .android_manifest
            .sdk
            .target_sdk_version
            .unwrap_or_else(|| self.ndk.default_target_platform());
        if let Err(e) = self.ndk.android_jar(target_sdk_version) {
            problems.push(format!(
                "target SDK {target_sdk_version}: {e}; install it with \
                 `sdkmanager \"platforms;android-{target_sdk_version}\"`"
            ));
        }

        let build_tools = self.ndk.build_tools_version().to_owned();
        let packaging_tools: &[&str] = match self.format {
            BuildFormat::Apk => &["aapt", "zipalign"],
            BuildFormat::Aab => &["aapt2"],
        };
        for tool in packaging_tools {
            if let Err(e) = self.ndk.build_tool_path(tool) {
                problems.push(format!("build-tools {build_tools}: {tool}: {e}"));
            }
        }

        let java_sources = self.java_sources()?;
        let mut missing_sources = false;
        for dir in &java_sources {
            if !dir.is_dir() {
                problems.push(format!(
                    "java_sources path `{}` does not exist",
                    dir.display()
                ));
                missing_sources = true;
            }
        }
        let (java_files, kt_files, jar_files) = if missing_sources {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            (
                collect_or_problem(collect_java_files, &java_sources, &mut problems),
                collect_or_problem(collect_kotlin_files, &java_sources, &mut problems),
                collect_or_problem(collect_jar_files, &java_sources, &mut problems),
            )
        };
        let no_sources = java_files.is_empty() && kt_files.is_empty() && jar_files.is_empty();

        if !java_sources.is_empty() && no_sources && !missing_sources {
            problems.push(format!(
                "java_sources hold no .java, .kt or .jar files: {}",
                join_display(&java_sources)
            ));
        }
        if !java_files.is_empty()
            && let Err(e) = self.ndk.javac()
        {
            problems.push(format!("javac: {e}"));
        }
        if !no_sources && let Err(e) = self.ndk.d8() {
            problems.push(format!("d8: {e}"));
        }
        // The roots are resolved together, not one at a time: version choice is
        // global, so resolving them separately can pick two versions of one
        // artifact where resolving them together picks one.
        if let Err(e) = rndk::maven::resolve_libs(&self.android_libs()?) {
            problems.push(e.to_string());
        }

        if !kt_files.is_empty() {
            match rndk::kotlin::resolve_toolchain() {
                Some(_) => {}
                None if rndk::kotlin::fetch_disabled() => problems.push(format!(
                    "kotlinc {} is unavailable and fetching is disabled; set \
                     CARGO_RAPK_KOTLINC or KOTLIN_HOME",
                    rndk::kotlin::kotlin_version()
                )),
                None => println!(
                    "note: kotlinc {} will be fetched into {}",
                    rndk::kotlin::kotlin_version(),
                    rndk::kotlin::kotlin_cache_dir().display()
                ),
            }
        }

        if !self.repro.unsigned {
            let (tool, resolved) = match self.format {
                BuildFormat::Apk => ("apksigner", self.ndk.apksigner().map(|_| ())),
                BuildFormat::Aab => ("jarsigner", self.ndk.jarsigner().map(|_| ())),
            };
            if let Err(e) = resolved {
                problems.push(format!("{tool}: {e}"));
            }
        }

        if let Some(sysroot) = rustc_sysroot(self.cmd.manifest()) {
            for target in &self.build_targets {
                let triple = target.rust_triple();
                if !sysroot.join("lib/rustlib").join(triple).is_dir() {
                    eprintln!(
                        "warning: no std for rust target `{triple}` in {}; \
                         run `rustup target add {triple}`",
                        sysroot.display()
                    );
                }
            }
        }

        if problems.is_empty() {
            let mut line =
                format!("preflight ok: build-tools {build_tools}, target SDK {target_sdk_version}");
            if !java_sources.is_empty() {
                line.push_str(&format!(
                    ", {} java source dirs ({} .java, {} .kt, {} .jar)",
                    java_sources.len(),
                    java_files.len(),
                    kt_files.len(),
                    jar_files.len()
                ));
            }
            let libs = self.android_libs()?;
            if !libs.is_empty() {
                let resolved = rndk::maven::resolve_libs(&libs)?;
                let with_res = resolved
                    .iter()
                    .filter(|lib| lib.resources.is_some())
                    .count();
                line.push_str(&format!(
                    ", {} android_libs resolving to {} artifacts ({with_res} with resources)",
                    libs.len(),
                    resolved.len()
                ));
            }
            println!("{line}");
            return Ok(());
        }

        Err(Error::Preflight(
            problems.into_iter().map(|p| format!("  - {p}")).collect(),
        ))
    }

    pub fn check(&self) -> Result<(), Error> {
        self.preflight()?;

        for target in &self.build_targets {
            let mut cargo = cargo_ndk(
                &self.ndk,
                *target,
                self.min_sdk_version(),
                self.cmd.target_dir(),
                self.repro.deterministic,
                if self.repro.deterministic {
                    self.repro.ts_unix
                } else {
                    None
                },
            )?;
            cargo.arg("check");
            if self.cmd.target().is_none() || self.universal {
                cargo.arg("--target").arg(target.rust_triple());
            }
            self.apply_cargo_args(&mut cargo);
            apply_manifest_features(&self.manifest, &mut cargo);
            if !cargo.status()?.success() {
                return Err(NdkError::CmdFailed(Box::new(cargo)).into());
            }
        }
        Ok(())
    }

    pub fn build(&self, artifact: &Artifact) -> Result<Apk, Error> {
        let mut manifest = self.manifest.android_manifest.clone();
        manifest.version_code = Some(self.version_code());
        if manifest.package.is_empty() {
            let name = artifact.name.replace('-', "_");
            manifest.package = match artifact.r#type {
                ArtifactType::Lib | ArtifactType::Bin => format!("rust.{name}"),
                ArtifactType::Example => format!("rust.example.{name}"),
            };
        }
        if manifest.application.label.is_empty() {
            manifest.application.label = artifact.name.to_string();
        }
        let crate_path = self.crate_path()?;

        let java_sources = self.java_sources()?;

        let contrib = collect_android_contributions(self.cmd.manifest())?;

        let mut existing_activity_names = manifest
            .application
            .activity
            .iter()
            .map(|activity| activity.name.clone())
            .collect::<HashSet<_>>();
        for activity in contrib.activities {
            if existing_activity_names.insert(activity.name.clone()) {
                manifest.application.activity.push(activity);
            }
        }

        let mut existing_service_names = manifest
            .application
            .service
            .iter()
            .map(|service| service.name.clone())
            .collect::<HashSet<_>>();
        for service in contrib.services {
            if existing_service_names.insert(service.name.clone()) {
                manifest.application.service.push(service);
            }
        }

        if manifest.application.activity.is_empty() {
            manifest
                .application
                .activity
                .push(rndk::manifest::Activity::default());
        }

        let has_main_action = manifest.application.activity.iter().any(|activity| {
            activity
                .intent_filter
                .iter()
                .any(|i| i.actions.iter().any(|f| f == "android.intent.action.MAIN"))
        });
        if !has_main_action {
            let activity_index = manifest
                .application
                .activity
                .iter()
                .position(|activity| activity.name == NATIVE_ACTIVITY_NAME)
                .unwrap_or(0);
            let activity = &mut manifest.application.activity[activity_index];
            activity.intent_filter.push(IntentFilter {
                actions: vec!["android.intent.action.MAIN".to_string()],
                categories: vec!["android.intent.category.LAUNCHER".to_string()],
                data: vec![],
            });
        }

        let target_sdk_version = manifest
            .sdk
            .target_sdk_version
            .unwrap_or_else(|| self.ndk.default_target_platform());
        if target_sdk_version >= 31 {
            for activity in &mut manifest.application.activity {
                if !activity.intent_filter.is_empty() {
                    activity.exported.get_or_insert(true);
                }
            }
        }

        let lib_name = artifact.name.replace('-', "_");
        let lib_name_meta = MetaData {
            name: "android.app.lib_name".to_string(),
            value: Some(lib_name),
            resource: None,
        };
        let mut attached_to_native_activity = false;
        for activity in &mut manifest.application.activity {
            if activity.name == NATIVE_ACTIVITY_NAME {
                activity.meta_data.push(lib_name_meta.clone());
                attached_to_native_activity = true;
            }
        }
        if !attached_to_native_activity {
            manifest
                .application
                .activity
                .first_mut()
                .ok_or(Error::NoActivities)?
                .meta_data
                .push(lib_name_meta);
        }

        let java_sources = dedup_paths(java_sources);
        let is_debug_profile = *self.cmd.profile() == Profile::Dev;
        let assets = self
            .manifest
            .assets
            .as_ref()
            .map(|p| dunce::simplified(&crate_path.join(p)).to_owned());
        let resources = self
            .manifest
            .resources
            .as_ref()
            .map(|p| dunce::simplified(&crate_path.join(p)).to_owned());
        let runtime_libs = self
            .manifest
            .runtime_libs
            .as_ref()
            .map(|p| dunce::simplified(&crate_path.join(p)).to_owned());
        let android_libs = self.android_libs()?;
        let mut lib_jars = Vec::new();
        let mut library_resources = Vec::new();
        let mut library_manifests = Vec::new();
        let mut r_libraries = Vec::new();
        for resolved in rndk::maven::resolve_libs(&android_libs)? {
            lib_jars.extend(resolved.jars.iter().cloned());
            if let Some(res) = &resolved.resources {
                library_resources.push(res.clone());
            }
            // `R` generation needs the library's own `package` and its
            // resources, and nothing else — a library whose manifest declares
            // no components still ships resources, and `appcompat` is the
            // common case: an empty manifest and 410 resource files.
            if resolved.resources.is_some()
                && let Some(package) = resolved.package.clone()
            {
                r_libraries.push((package, String::new()));
            }
            // Manifest merging is independent of the above, so it cannot be
            // gated on the manifest having survived the contributions filter.
            library_manifests.extend(resolved.manifest);
        }
        lib_jars.sort();
        lib_jars.dedup();
        library_resources.sort();
        library_resources.dedup();
        library_manifests.sort();
        library_manifests.dedup();
        r_libraries.sort();
        r_libraries.dedup();

        if !java_sources.is_empty() || !lib_jars.is_empty() {
            manifest.application.has_code = true;
        }
        let apk_name = self
            .manifest
            .apk_name
            .clone()
            .unwrap_or_else(|| artifact.name.to_string());
        // Uncompressed libraries are `mmap`ed straight out of the APK, which
        // requires `extractNativeLibs="false"`; a release build that claims
        // otherwise is invalid.
        let disable_aapt_compression =
            is_debug_profile || manifest.application.extract_native_libs == Some(false);

        let config = ApkConfig {
            ndk: self.ndk.clone(),
            build_dir: self.build_dir.join(artifact.build_dir()),
            apk_name,
            assets,
            resources,
            library_resources,
            library_manifests,
            manifest,
            disable_aapt_compression,
            strip: self.manifest.strip,
            reverse_port_forward: self.manifest.reverse_port_forward.clone(),
            format: self.format,
            align: self.repro.align,
            normalize_zip: self.repro.deterministic && !self.repro.no_normalize_zip,
            zip_timestamp: self.repro.ts_unix,
        };
        let mut apk = config.create_apk()?;

        // A library's `R` class has to be generated from the same overlays the
        // app is linked against, so this reuses the compiled tree `create_apk`
        // already produced.
        let mut r_classes = Vec::new();
        let mut extra_classpath = Vec::new();
        if !r_libraries.is_empty() {
            let overlays = config.compiled_overlays()?;
            let r_dir = config.build_dir.join("lib_r");
            let r_java = rndk::maven::generate_library_r(
                &self.ndk,
                &overlays,
                &r_libraries,
                self.min_sdk_version(),
                target_sdk_version,
                &r_dir,
            )?;
            let r_class_dir = r_dir.join("classes");
            r_classes = compile_r_java(
                &self.ndk,
                &r_java,
                &lib_jars,
                &r_class_dir,
                &config.build_dir,
            )?;
            // The generated `R` classes have to be on the classpath the app's
            // own sources compile against, or `androidx.appcompat.R.id.x` does
            // not resolve.
            extra_classpath.push(r_class_dir);
        }

        if !java_sources.is_empty() || !lib_jars.is_empty() || !r_classes.is_empty() {
            let dex_files = compile_java_sources(
                &self.ndk,
                java_sources.as_slice(),
                DexInputs {
                    lib_jars: lib_jars.as_slice(),
                    classes: r_classes.as_slice(),
                    classpath: extra_classpath.as_slice(),
                },
                &config.build_dir,
                self.min_sdk_version(),
                target_sdk_version,
            )?;
            for dex_file in dex_files {
                let file_name = dex_file
                    .file_name()
                    .ok_or_else(|| NdkError::PathNotFound(dex_file.clone()))?;
                apk.add_file(&dex_file, Path::new(file_name))?;
            }
        }

        for target in &self.build_targets {
            let triple = target.rust_triple();
            let build_dir = self.cmd.build_dir(Some(triple));
            let artifact = self.cmd.artifact(artifact, Some(triple), CrateType::Cdylib);

            let mut cargo = cargo_ndk(
                &self.ndk,
                *target,
                self.min_sdk_version(),
                self.cmd.target_dir(),
                self.repro.deterministic,
                if self.repro.deterministic {
                    self.repro.ts_unix
                } else {
                    None
                },
            )?;
            cargo.arg("build");
            if self.cmd.target().is_none() || self.universal {
                cargo.arg("--target").arg(triple);
            }
            self.apply_cargo_args(&mut cargo);
            apply_manifest_features(&self.manifest, &mut cargo);
            if !cargo.status()?.success() {
                return Err(NdkError::CmdFailed(Box::new(cargo)).into());
            }

            let mut libs_search_paths = rndk::dylibs::get_libs_search_paths(
                self.cmd.target_dir(),
                triple,
                self.cmd.profile().as_ref(),
            )?;
            libs_search_paths.push(build_dir.join("deps"));
            libs_search_paths.sort();

            // Register every library the caller named before resolving, so that
            // precedence cannot depend on which one is reached first.
            let mut libs = LibResolver::new(
                &self.ndk,
                *target,
                self.min_sdk_version(),
                libs_search_paths,
            )?;
            libs.register(LibSource::MainLib, &artifact)?;
            if let Some(runtime_libs) = &runtime_libs {
                libs.register_dir(
                    LibSource::Explicit,
                    &runtime_libs.join(target.android_abi()),
                )?;
            }
            apk.add_libs(&mut libs, *target)?;
        }

        let unsigned = apk.add_pending_libs_and_align()?;

        if self.repro.unsigned {
            let ext = match self.format {
                BuildFormat::Apk => "APK",
                BuildFormat::Aab => "AAB",
            };
            eprintln!(
                "--unsigned set; producing unsigned {ext} at {}",
                config.output_path().display()
            );
            return Ok(rndk::apk::Apk::from_config(unsigned.config()));
        }

        // normal signing flow
        let profile_name = match self.cmd.profile() {
            Profile::Dev => "dev",
            Profile::Release => "release",
            Profile::Custom(c) => c.as_str(),
        };
        let keystore_env = format!(
            "CARGO_RAPK_{}_KEYSTORE",
            profile_name.to_uppercase().replace('-', "_")
        );
        let password_env = format!("{keystore_env}_PASSWORD");
        let alias_env = format!("{keystore_env}_ALIAS");
        let key_password_env = format!("{keystore_env}_KEY_PASSWORD");
        let path = std::env::var_os(&keystore_env).map(PathBuf::from);
        let password = std::env::var(&password_env).ok();
        let key_password = std::env::var(&key_password_env)
            .ok()
            .filter(|p| !p.is_empty());
        let alias = std::env::var(&alias_env)
            .ok()
            .filter(|a| !a.is_empty())
            .or_else(|| {
                self.manifest
                    .signing
                    .get(profile_name)
                    .and_then(|msk| msk.alias.clone())
            });
        let signing_key = match (path, password) {
            (Some(path), Some(password)) => Key {
                path,
                password,
                alias,
                key_password,
            },
            (Some(path), None) if *self.cmd.profile() == Profile::Dev => Key {
                path,
                password: rndk::ndk::DEFAULT_DEV_KEYSTORE_PASSWORD.to_owned(),
                alias,
                key_password,
            },
            (Some(_path), None) => {
                return Err(Error::MissingKeystorePassword(profile_name.into()));
            }
            (None, _) => {
                if let Some(msk) = self.manifest.signing.get(profile_name) {
                    Key {
                        path: crate_path.join(&msk.path),
                        password: msk.keystore_password.clone(),
                        alias: msk.alias.clone(),
                        key_password: msk.key_password.clone().or(key_password),
                    }
                } else if *self.cmd.profile() == Profile::Dev {
                    self.ndk.debug_key()?
                } else {
                    return Err(Error::MissingReleaseKey(profile_name.to_owned()));
                }
            }
        };

        if self.format == BuildFormat::Aab && signing_key.alias.is_none() {
            return Err(Error::MissingKeystoreAlias {
                profile_env: profile_name.into(),
                profile: profile_name.to_owned(),
            });
        }

        println!(
            "Signing `{}` with keystore `{}`",
            config.output_path().display(),
            signing_key.path.display()
        );
        Ok(unsigned.sign(signing_key)?)
    }

    pub fn run(&self, artifact: &Artifact, no_logcat: bool) -> Result<(), Error> {
        let apk = self.build(artifact)?;
        apk.reverse_port_forwarding(self.device_serial.as_deref())?;
        apk.install(self.device_serial.as_deref())?;
        apk.start(self.device_serial.as_deref())?;
        let uid = apk.uidof(self.device_serial.as_deref())?;

        if !no_logcat {
            self.ndk
                .adb(self.device_serial.as_deref())?
                .arg("logcat")
                .arg("-v")
                .arg("color")
                .arg("--uid")
                .arg(uid.to_string())
                .status()?;
        }

        Ok(())
    }

    pub fn gdb(&self, artifact: &Artifact) -> Result<(), Error> {
        let apk = self.build(artifact)?;
        apk.install(self.device_serial.as_deref())?;

        let target_dir = self.build_dir.join(artifact.build_dir());
        self.ndk.ndk_gdb(
            target_dir,
            NATIVE_ACTIVITY_NAME,
            self.device_serial.as_deref(),
        )?;
        Ok(())
    }

    pub fn default(&self, cargo_cmd: &str, cargo_args: &[String]) -> Result<(), Error> {
        for target in &self.build_targets {
            let mut cargo = cargo_ndk(
                &self.ndk,
                *target,
                self.min_sdk_version(),
                self.cmd.target_dir(),
                self.repro.deterministic,
                if self.repro.deterministic {
                    self.repro.ts_unix
                } else {
                    None
                },
            )?;
            cargo.arg(cargo_cmd);
            self.apply_cargo_args(&mut cargo);
            apply_manifest_features(&self.manifest, &mut cargo);

            if self.cmd.target().is_none() || self.universal {
                let triple = target.rust_triple();
                cargo.arg("--target").arg(triple);
            }

            for additional_arg in cargo_args {
                cargo.arg(additional_arg);
            }

            if !cargo.status()?.success() {
                return Err(NdkError::CmdFailed(Box::new(cargo)).into());
            }
        }
        Ok(())
    }

    fn version_code(&self) -> u32 {
        let base = i64::from(self.base_version_code);
        let offset = if self.universal {
            1
        } else if let [target] = self.build_targets.as_slice() {
            i64::from(target.version_code_offset())
        } else {
            0
        };
        base.saturating_add(offset).max(1) as u32
    }

    /// Returns `minSdkVersion` for use in compiler target selection:
    /// <https://developer.android.com/ndk/guides/sdk-versions#minsdkversion>
    ///
    /// Has a lower bound of `23` to retain backwards compatibility with
    /// the previous default.
    fn min_sdk_version(&self) -> u32 {
        self.manifest
            .android_manifest
            .sdk
            .min_sdk_version
            .unwrap_or(23)
            .max(23)
    }
}

fn collect_or_problem(
    collect: fn(&[PathBuf]) -> Result<Vec<PathBuf>, Error>,
    dirs: &[PathBuf],
    problems: &mut Vec<String>,
) -> Vec<PathBuf> {
    match collect(dirs) {
        Ok(files) => files,
        Err(e) => {
            problems.push(e.to_string());
            Vec::new()
        }
    }
}

fn join_display(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn rustc_sysroot(manifest_path: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(manifest_path.parent()?)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sysroot = String::from_utf8(output.stdout).ok()?;
    let sysroot = sysroot.trim();
    (!sysroot.is_empty()).then(|| PathBuf::from(sysroot))
}

fn apply_manifest_features(manifest: &Manifest, cargo: &mut std::process::Command) {
    if !manifest.features.is_empty() {
        cargo.arg("--features");
        cargo.arg(manifest.features.join(","));
    }
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::dedup_paths;
    use std::path::PathBuf;

    #[test]
    fn dedup_paths_preserves_first_occurrence_order() {
        let input = vec![
            PathBuf::from("a"),
            PathBuf::from("b"),
            PathBuf::from("a"),
            PathBuf::from("c"),
            PathBuf::from("b"),
        ];

        let output = dedup_paths(input);
        assert_eq!(
            output,
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")]
        );
    }
}
