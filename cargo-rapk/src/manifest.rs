use crate::error::Error;
use rndk::apk::StripConfig;
use rndk::manifest::AndroidManifest;
use rndk::target::Target;
use serde::{Deserialize, Deserializer};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Inheritable<T> {
    Value(T),
    Inherited { workspace: bool },
}

pub(crate) struct Manifest {
    pub(crate) version: Inheritable<String>,
    pub(crate) apk_name: Option<String>,
    pub(crate) android_manifest: AndroidManifest,
    pub(crate) build_targets: Vec<Target>,
    pub(crate) assets: Option<PathBuf>,
    pub(crate) resources: Option<PathBuf>,
    pub(crate) java_sources: Vec<PathBuf>,
    pub(crate) runtime_libs: Option<PathBuf>,
    /// Maps profiles to keystores
    pub(crate) signing: HashMap<String, Signing>,
    pub(crate) reverse_port_forward: HashMap<String, String>,
    pub(crate) strip: StripConfig,
}

impl Manifest {
    pub(crate) fn parse_from_toml(path: &Path) -> Result<Self, Error> {
        let toml = Root::parse_from_toml(path)?;
        // Unlikely to fail as cargo-subcommand should give us a `Cargo.toml` containing
        // a `[package]` table (with a matching `name` when requested by the user)
        let package = toml
            .package
            .unwrap_or_else(|| panic!("Manifest `{:?}` must contain a `[package]`", path));
        let metadata = package
            .metadata
            .unwrap_or_default()
            .android
            .unwrap_or_default();
        Ok(Self {
            version: package.version,
            apk_name: metadata.apk_name,
            android_manifest: metadata.android_manifest,
            build_targets: metadata.build_targets,
            assets: metadata.assets,
            resources: metadata.resources,
            java_sources: metadata.java_sources,
            runtime_libs: metadata.runtime_libs,
            signing: metadata.signing,
            reverse_port_forward: metadata.reverse_port_forward,
            strip: metadata.strip,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Root {
    pub(crate) package: Option<Package>,
    pub(crate) workspace: Option<Workspace>,
}

impl Root {
    pub(crate) fn parse_from_toml(path: &Path) -> Result<Self, Error> {
        let contents = std::fs::read_to_string(path)?;
        toml::from_str(&contents).map_err(|e| e.into())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Package {
    pub(crate) version: Inheritable<String>,
    pub(crate) metadata: Option<PackageMetadata>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Workspace {
    pub(crate) package: Option<WorkspacePackage>,
}

/// Almost the same as [`Package`], except that this must provide
/// root values instead of possibly inheritable values
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct WorkspacePackage {
    pub(crate) version: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct PackageMetadata {
    android: Option<AndroidMetadata>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AndroidMetadata {
    apk_name: Option<String>,
    #[serde(flatten)]
    android_manifest: AndroidManifest,
    #[serde(default)]
    build_targets: Vec<Target>,
    assets: Option<PathBuf>,
    resources: Option<PathBuf>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_one_or_many")]
    java_sources: Vec<PathBuf>,
    runtime_libs: Option<PathBuf>,
    /// Maps profiles to keystores
    #[serde(default)]
    signing: HashMap<String, Signing>,
    /// Set up reverse port forwarding before launching the application
    #[serde(default)]
    reverse_port_forward: HashMap<String, String>,
    #[serde(default)]
    strip: StripConfig,
}

pub(crate) fn deserialize_one_or_many<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        One(T),
        Many(Vec<T>),
    }

    match OneOrMany::<T>::deserialize(deserializer)? {
        OneOrMany::One(value) => Ok(vec![value]),
        OneOrMany::Many(values) => Ok(values),
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct Signing {
    pub(crate) path: PathBuf,
    pub(crate) keystore_password: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_android_metadata(toml_source: &str) -> AndroidMetadata {
        let root: Root = toml::from_str(toml_source).expect("failed to parse manifest toml");
        let package = root.package.expect("missing package table");
        package
            .metadata
            .expect("missing package metadata")
            .android
            .expect("missing android metadata")
    }

    #[test]
    fn parses_single_activity_table() {
        let metadata = parse_android_metadata(
            r#"
                [package]
                version = "0.1.0"

                [package.metadata.android.application.activity]
                name = "android.app.NativeActivity"
            "#,
        );

        assert_eq!(metadata.android_manifest.application.activity.len(), 1);
        assert_eq!(
            metadata.android_manifest.application.activity[0].name,
            "android.app.NativeActivity"
        );
    }

    #[test]
    fn parses_multiple_activity_tables() {
        let metadata = parse_android_metadata(
            r#"
                [package]
                version = "0.1.0"

                [[package.metadata.android.application.activity]]
                name = "android.app.NativeActivity"

                [[package.metadata.android.application.activity]]
                name = "rust.rlobkit.RlobKitPickerActivity"
                exported = false
            "#,
        );

        assert_eq!(metadata.android_manifest.application.activity.len(), 2);
        assert_eq!(
            metadata.android_manifest.application.activity[1].name,
            "rust.rlobkit.RlobKitPickerActivity"
        );
    }

    #[test]
    fn parses_java_sources_single_and_multiple() {
        let single = parse_android_metadata(
            r#"
                [package]
                version = "0.1.0"

                [package.metadata.android]
                java_sources = "android"
            "#,
        );
        assert_eq!(single.java_sources, vec![PathBuf::from("android")]);

        let multiple = parse_android_metadata(
            r#"
                [package]
                version = "0.1.0"

                [package.metadata.android]
                java_sources = ["android", "third_party/android"]
            "#,
        );
        assert_eq!(
            multiple.java_sources,
            vec![
                PathBuf::from("android"),
                PathBuf::from("third_party/android")
            ]
        );
    }

    #[test]
    fn parses_cargo_rapk_contribution_metadata() {
        #[derive(Deserialize)]
        struct RootMeta {
            package: PackageMeta,
        }

        #[derive(Deserialize)]
        struct PackageMeta {
            metadata: PackageAndroidMeta,
        }

        #[derive(Deserialize)]
        struct PackageAndroidMeta {
            android: PackageCargoRapkMeta,
        }

        #[derive(Deserialize)]
        struct PackageCargoRapkMeta {
            cargo_rapk: ContributionMeta,
        }

        #[derive(Deserialize)]
        struct ContributionMeta {
            #[serde(default)]
            #[serde(deserialize_with = "deserialize_one_or_many")]
            java_sources: Vec<PathBuf>,
            #[serde(default)]
            #[serde(deserialize_with = "deserialize_one_or_many")]
            activities: Vec<rndk::manifest::Activity>,
        }

        let manifest: RootMeta = toml::from_str(
            r#"
                [package]
                version = "0.1.0"

                [package.metadata.android.cargo_rapk]
                java_sources = "android"

                [[package.metadata.android.cargo_rapk.activities]]
                name = "rust.rlobkit.RlobKitPickerActivity"
                exported = false
            "#,
        )
        .expect("failed to parse cargo_rapk contribution metadata");

        assert_eq!(
            manifest.package.metadata.android.cargo_rapk.java_sources,
            vec![PathBuf::from("android")]
        );
        assert_eq!(
            manifest
                .package
                .metadata
                .android
                .cargo_rapk
                .activities
                .len(),
            1
        );
    }
}
