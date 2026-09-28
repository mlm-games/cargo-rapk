# 0.25.0 (2026-09-28)
- **Fix:** Let an `android_libs` entry say what it is, with a `!platform` or `!library` suffix. Gradle reads a platform off the declaration — `platform("...")` versus a plain one — and never off the module: `JavaEcosystemVariantDerivationStrategy` derives both a library and a platform variant, and only the library one is `.withoutConstraints()`. `<packaging>` cannot say which was meant, so the entry can: `!platform` makes it a platform whatever its packaging, and `!library` makes it a library whose `<dependencyManagement>` is not applied to the graph, which is what a `pom` aggregator named as an ordinary dependency gets in Gradle. An unmarked entry is still guessed from the packaging, so existing entries resolve as they did.
- **Fix:** Key a relocated module by the coordinates a dependency names and hang the target off it as an edge, rather than re-keying the graph onto the target. `GradlePomModuleDescriptorParser` marks the module relocated, so `RealisedMavenModuleResolveMetadata.getArtifactsForConfiguration` gives it no artifacts of its own, and the target enters as an ordinary dependency. Re-keying put a relocated claim and an un-relocated one under different names, so mediation never compared them and one APK shipped both.
- **Fix:** Refuse a relocation to another version of the same coordinates. Gradle logs an error, takes only the target's dependencies and ships no artifact; the other half of that is an APK whose classes are missing, which only a device finds. The error names the version to list instead.
- **Fix:** Take a `pom`-packaged module's own jar when the repository publishes one, as `RealisedMavenModuleResolveMetadata` does through `optionalArtifact`, and treat its absence as "not published" rather than a failure. Maven suppresses the probe outright, so a `pom` aggregator that does ship a jar was missing from the classpath.
- **Fix:** Hand a repository's token and password to `curl` on its standard input instead of as arguments. Every argument a running process was given is readable through `/proc/<pid>/cmdline`, so `-H "Authorization: Bearer ..."` published the secret for as long as the download ran.
- **Fix:** Reject a compile-only `apiElements` variant on a runtime classpath. Gradle's `UsageCompatibilityRules` accepts a `java-runtime` producer for a `java-api` request but not the reverse; reading usage as a flat set of acceptable values lost that direction, and a variant-matching-count step then preferred `apiElements` for declaring more of the requested attributes than `runtimeElements` beside it. Both are removed: Gradle has no count step, and its disambiguation order is the requested attributes, then a variant that declares an extra attribute, then one that lacks it. Verified against `MultipleCandidateMatcher.disambiguateCompatibleCandidates`.
- **Fix:** Compute the extra-attribute set as the union over all candidates, dropping any name every candidate has, as Gradle's `collectExtraAttributes` does. Reading it off one candidate missed a name only a later variant carried, so two variants of a shadow-jar publisher came out indistinguishable and the build failed with "cannot choose between variants".
- **Fix:** Read an attribute value that is a boolean or a number, as the specification allows. `androidx.annotation:annotation:1.3.0` writes `"org.gradle.jvm.version": 8` unquoted, and a string-only field discarded that module's whole metadata and silently fell back to its POM.
- **Fix:** Rewrite a deprecated `java-runtime-jars` usage to `java-runtime` before matching, as Gradle's `UsageCompatibilityHandler` does, so a module still published the old way is a runtime variant rather than one that matches nothing.
- **Fix:** Settle `org.gradle.jvm.version` before `org.jetbrains.kotlin.platform.type`. The latter is absent from Gradle's `attributeDisambiguationPrecedence`, so it can never be disambiguated first.
- **Fix:** Honour a GMM `rejects` list. Gradle filters a candidate through `rejectedVersionsSelector` and rejects a static version too, so a module published only at a rejected version fails the build naming it. A published rejection is how a known-broken version is kept out of a classpath, and reporting it without obeying it shipped the very version the publisher excluded.
- **Fix:** Treat `<scope>import</scope>` on its own as an import. Gradle's `isDependencyImportScoped` tests nothing else, and requiring `<type>pom</type>` discarded a working BOM's versions.
- **Fix:** Merge `dependencyManagement` as `local > import > parent`, the order Gradle's `PomReader.resolveDependencyMgt` applies them in, so an imported BOM outranks an inherited version. Resolve two imports of the same module to the first declared, which a `BTreeMap` made alphabetical.
- **Fix:** Do not turn a platform's own import entries into graph-wide constraints. Gradle's `addDependencies` skips import-scoped entries, and including one pinned a directly-requested module to the BOM's version.
- **Fix:** Reject a capability only when two resolved artifacts provide the same one *and* they agree on a version. Gradle's `UpgradeCapabilityResolver` resolves a version disagreement to the highest, and `LastCandidateCapabilityResolver` has nothing to choose between when they agree, so only that case is fatal. A conflict is registered by two providers, with no `requestedCapabilities` test in that path.
- **Fix:** Default the log filter to `warn` instead of `error`. Every diagnostic worth seeing is a warning — a library declaration that could not be carried into the manifest, a hard range the resolution does not satisfy, a download with no checksum to verify it against — and a warning nobody can see is the same as no warning at all. `RUST_LOG` still wins.
- **Fix:** Check the Kotlin toolchain's own stdlib for duplicate classes alongside the `android_libs` jars. A Kotlin app that also lists `kotlin-stdlib` had its duplicate found only by `d8`, which is the raw report this check exists to replace.
- Report a repository search that finds nothing for a group, listing the configured repositories and their group filters, rather than ending with an empty list of attempts.
- Read `java.specification.version` correctly, so `org.gradle.jvm.version` is matched against the JDK the build really has rather than silently against Java 8. Read wrong, every variant needing a newer JVM was rejected, which for a Kotlin Multiplatform module meant shipping the metadata stub instead of the `-jvm` artifact.
- Report a duplicate class only for a program class. `module-info.class` sits at the jar root and a multi-release overlay under `META-INF/versions/`; both are JPMS descriptors `d8` does not dex, and either one being reported failed a build that dexes fine.
- Name the artifact, not the version directory, in a duplicate-class report; the report exists to be read.
- Fail a `pom`-packaged module's own dependencies no longer being resolved. A `pom` module that merely imports a BOM is an aggregator, and treating it purely as a platform dropped its whole subtree.
- Apply a platform that publishes its versions only as `dependencyConstraints` in its `.module`, which previously made it a no-op, and only treat a module as a platform when nothing else it offers is a usable library, so a BOM that also ships a jar is still a library.
- Keep a repository's `include_groups` and `exclude_groups` from being bypassed when no repository serves a group. Falling back to all of them sent a group to a repository configured never to serve it, credentials included.
- Accept a `maven_repositories` entry that gives only `url`. Only `url` is required now; every other field is optional, and a missing one previously aborted the whole `Cargo.toml` read, taking the entire manifest with it.
- Expand `${NAME}` in the `CARGO_RAPK_MAVEN_REPOSITORIES`, `CARGO_RAPK_MAVEN_GOOGLE` and `CARGO_RAPK_MAVEN_CENTRAL` forms as the `Cargo.toml` one does, and leave an unterminated `${` as written rather than repeating the text before it.
- Reuse a cached download from a repository that publishes no `.sha1`, instead of re-fetching it on every build.

- **Breaking:** A `pom`-packaged `android_libs` entry is a platform: it coordinates versions for the whole graph instead of adding a jar, so it moves an artifact another path already put on the classpath. This is what fixes the closure that could not be resolved — `kotlin-stdlib` 1.8 absorbed the `jdk7`/`jdk8` internals, so the `kotlin-stdlib-jdk7:1.6.21` that `kotlinx-coroutines` asks for contributes classes `kotlin-stdlib:1.8.22` already has, and no amount of version mediation across two different modules reconciles that. Add `org.jetbrains.kotlin:kotlin-bom:<version>!platform` and it resolves; a platform that nothing wants is still never pulled in.
- Report a class that two `android_libs` artifacts both define, and the two artifacts that define it, instead of leaving `d8` to print the whole classpath with the conflict buried in it. A `module-info.class` and a multi-release overlay are excluded, being JPMS descriptors that `d8` does not dex.
- Report a library whose own `uses-sdk` asks for a higher `minSdkVersion` than the app declares, which nothing would have enforced until the app failed on an older device.
- Search every repository in order rather than choosing one from the group prefix, which was wrong for `com.google.firebase`: it is on Google Maven and not on Central, so the Firebase libraries could not be fetched at all. A repository that publishes no `.sha1` sidecar for a file, which Google Maven does not for every POM, is now reported and the download goes ahead; a sidecar that is present and disagrees still fails.
- **Breaking:** Carry the elements and attributes a library declares that were being dropped, several of which change how the app behaves on a device: a permission's `android:protectionLevel`, so a `signature` permission no longer becomes a `normal` one any app can hold; an intent filter's `<data>` deep links; `android:appComponentFactory`, without which the component factory never initialises; `android:theme` on a library activity; `uses-library`; `android:priority` on a filter; `directBootAware` on a provider; and `excludeFromRecents`, `fitsSystemWindows` and `stateNotNeeded` on an activity.
- Report a library element or attribute that is declared and not modelled, rather than dropping it silently. `tools:` directives and `xmlns:` declarations are not reported, being build-time and namespace rather than application state.
- Add `[[package.metadata.android.maven_repositories]]`, with `url`, `username`, `password`, `token`, `include_groups` and `exclude_groups`, so an internal Nexus or a mirror can be searched first. `${NAME}` in a field is read from the environment, keeping a secret out of `Cargo.toml`. `CARGO_RAPK_MAVEN_GOOGLE` and `CARGO_RAPK_MAVEN_CENTRAL` mirror the defaults globally.
- Honour `org.gradle.jvm.version` when choosing a variant, against the JDK the build already needs, so a library published for a newer Java is rejected rather than reached past.
- Read a dependency's `thirdPartyCompatibility.artifactSelector`, so an artifact published under a classifier is fetched as that artifact instead of the default one.

- **Breaking:** Resolve `android_libs` transitively. A list that named every artifact by hand now resolves its own closure, following POMs and Gradle Module Metadata, so `androidx.appcompat:appcompat:1.7.0` alone pulls 43 artifacts. Where two paths want different versions of one artifact the newest wins whatever its distance from the root, matching Gradle. A hand-written list therefore resolves to a different, larger set than it did before, and an explicit pin can be superseded by a newer transitive request; declare the version you want and check the build's resolved list. A hard range `[x,y]` that the winner cannot satisfy is reported as a warning rather than failing the build, which is a deliberate divergence from Gradle rather than parity: Gradle fails such a resolution, and one skewed patch level in the AndroidX POM set would otherwise make a whole library unusable.
- **Breaking:** Build the APK with `aapt2` instead of `aapt` v1. `resources.arsc` differs from any build made before this change, so a reference APK produced by an older release will not match a rebuild of the same source; rebuild reference APKs when upgrading. Inputs `aapt` v1 accepted, such as an overlay-only resource, are now rejected.
- **Breaking:** Merge library manifests for real. They previously were parsed into nothing, so an APK built with `android_libs` shipped without anything the library declared. It now carries the library's `uses-permission`, `permission`, `uses-feature`, `grant-uri-permission`, `meta-data`, and its `activity`, `service`, `receiver` and `provider` with their intent filters, with `${applicationId}` substituted. A library component whose `tools:node` is `remove` is dropped and one marked `replace` overrides the app's declaration; otherwise the app's own declaration wins. Apps that previously shipped with these declarations missing will now declare them, which can surface a `SecurityException` or a missing-component failure that the gap was hiding.
- **Breaking:** `android:enabled` keeps a resource reference such as `@bool/enable_system_alarm_service_default` instead of dropping it, so a component a library gates on an API-dependent default is no longer shipped enabled. `aapt2` now fails the build if such a reference does not resolve, where the attribute used to be discarded.
- `queries.provider.name` is optional, as the specification has it; it was only required while the APK path ran `aapt` v1.
- Fix library `res/` being extracted to a nested `res/res/` and so compiled as an empty tree, which left every library resource out of `resources.arsc` and every generated `R` class empty.
- Read self-closing components such as `<service ... />`, which were skipped; `androidx.work` declares its services that way, and they were dropped along with its receivers, so WorkManager jobs could never run.
- Do not write a `tools:` attribute into the merged manifest, which has no `tools` namespace declared and so could not be parsed by `aapt2`.

# 0.24.0 (2026-09-27)

- **Breaking:** Default `target_sdk_version` to `35` (if installed), matching Google Play requirements starting August 31 2025.
- Accept a `Cargo.toml` without a `package.version` field, which Cargo has made optional since 1.75, defaulting it to `0.0.0` as Cargo does. ([#56](https://github.com/rust-mobile/cargo-apk/pull/56))
- Store libraries uncompressed when `application.extract_native_libs` is `false`, since the installer only `mmap`s uncompressed libraries out of the APK; a release build that claimed otherwise produced an invalid APK. ([#32](https://github.com/rust-mobile/cargo-apk/issues/32))
- Support the `receiver` element and the `uses-native-library`, `profileable` and `install_location` manifest options. ([#85](https://github.com/rust-mobile/cargo-apk/pull/85), [#87](https://github.com/rust-mobile/cargo-apk/pull/87), [#58](https://github.com/rust-mobile/cargo-apk/pull/58))
- Support the `request_legacy_external_storage` and `allow_native_heap_pointer_tagging` manifest options. ([#82](https://github.com/rust-mobile/cargo-apk/pull/82))
- Bump `rndk` with 16KiB page alignment, Cargo configuration `rustflags` support, current-user `adb` operations and minor API level platform detection.
- Report a manifest path with no parent directory, and a manifest with no activity to attach `android.app.lib_name` to, as errors instead of panicking.
- **Breaking:** `build.rustflags` and `target.<triple>.rustflags` from the Cargo configuration are now applied to Android builds, as they already were for host builds; set `RUSTFLAGS` to keep overriding the configuration.

# 0.10.0 (2023-11-30)

- Bump MSRV to 1.70 to reflect dependency updates.
- Bump `rndk` to [`0.10.0`](https://github.com/mlm-games/cargo-rapk/releases/tag/rndk-0.10.0) with various fixes:
  - Improved log filtering based on UID instead of PID;
  - Support for building APKs from an Android host;
  - More manifest attributes are now supported on the `Application` and `Activity` elements.
- Improved artifacts based on https://github.com/mlm-games/cargo-subcommand/pull/17 to support renames of `[lib]`, `[[bin]]` and `[[example]]`. ([#26](https://github.com/mlm-games/cargo-rapk/pull/26))

# 0.9.7 (2022-12-12)

- Reimplement default package selection based on `$PWD` or `--manifest-path` by upgrading to [`cargo-subcommand 0.11.0`](https://github.com/mlm-games/cargo-subcommand/releases/tag/0.11.0). ([#4](https://github.com/mlm-games/cargo-rapk/pull/4))
- Removed known-arg parsing from `cargo apk --` to not make argument flags/values get lost, see also [#375](https://github.com/rust-windowing/android-ndk-rs/issues/375). ([#377](https://github.com/rust-windowing/android-ndk-rs/pull/377))

# 0.9.6 (2022-11-23)

- Profile signing information can now be specified via the `CARGO_RAPK_<PROFILE>_KEYSTORE` and `CARGO_RAPK_<PROFILE>_KEYSTORE_PASSWORD` environment variables. The environment variables take precedence over signing information in the cargo manifest. Both environment variables are required except in the case of the `dev` profile, which will fall back to the default password if `CARGO_RAPK_DEV_KEYSTORE_PASSWORD` is not set. ([#358](https://github.com/rust-windowing/android-ndk-rs/pull/358))
- Add `strip` option to `android` metadata, allowing a user to specify how they want debug symbols treated after cargo has finished building, but before the shared object is copied into the APK. ([#356](https://github.com/rust-windowing/android-ndk-rs/pull/356))
- Support [`[workspace.package]` inheritance](https://doc.rust-lang.org/cargo/reference/workspaces.html#the-workspacepackage-table) from a workspace root manifest for the `version` field under `[package]`. ([#360](https://github.com/rust-windowing/android-ndk-rs/pull/360))

(0.9.5, released on 2022-10-14, was yanked due to unintentionally bumping MSRV through the `quick-xml` crate, and breaking `cargo apk --` parsing after switching to `clap`.)

- Update to `cargo-subcommand 0.8.0` with `clap` argument parser. ([#238](https://github.com/rust-windowing/android-ndk-rs/pull/238))
- Automate `adb reverse` port forwarding through `Cargo.toml` metadata ([#348](https://github.com/rust-windowing/android-ndk-rs/pull/348))

# 0.9.4 (2022-09-12)

- Upgrade to latest `rndk` to deduplicate libraries before packaging them into the APK. ([#333](https://github.com/rust-windowing/android-ndk-rs/pull/333))
- Support `android:resizeableActivity`. ([#338](https://github.com/rust-windowing/android-ndk-rs/pull/338))
- Add `--device` argument to select `adb` device by serial (see `adb devices` for connected devices and their serial). ([#329](https://github.com/rust-windowing/android-ndk-rs/pull/329))
- Print and follow `adb logcat` output after starting app. ([#332](https://github.com/rust-windowing/android-ndk-rs/pull/332))

# 0.9.3 (2022-07-05)

- Allow configuration of alternate debug keystore location; require keystore location for release builds. ([#299](https://github.com/rust-windowing/android-ndk-rs/pull/299))
- **Breaking:** Rename `Activity::intent_filters` back to `Activity::intent_filter`. ([#305](https://github.com/rust-windowing/android-ndk-rs/pull/305))

# 0.9.2 (2022-06-11)

- Move NDK r23 `-lgcc` workaround to `ndk_build::cargo::cargo_ndk()`, to also apply to our `cargo apk --` invocations. ([#286](https://github.com/rust-windowing/android-ndk-rs/pull/286))
- Disable `aapt` compression for the [(default) `dev` profile](https://doc.rust-lang.org/cargo/reference/profiles.html). ([#283](https://github.com/rust-windowing/android-ndk-rs/pull/283))
- Append `--target` to blanket `cargo apk --` calls when not provided by the user. ([#287](https://github.com/rust-windowing/android-ndk-rs/pull/287))

# 0.9.1 (2022-05-12)

- Reimplement NDK r23 `-lgcc` workaround using `RUSTFLAGS`, to apply to transitive `cdylib` compilations. (#270)

# 0.9.0 (2022-05-07)

- **Breaking:** Use `min_sdk_version` to select compiler target instead of `target_sdk_version`. ([#197](https://github.com/rust-windowing/android-ndk-rs/pull/197))
  See <https://developer.android.com/ndk/guides/sdk-versions#minsdkversion> for more details.
- **Breaking:** Default `target_sdk_version` to `30` or lower (instead of the highest supported SDK version by the detected NDK toolchain)
  for more consistent interaction with Android backwards compatibility handling and its increasingly strict usage rules:
  <https://developer.android.com/distribute/best-practices/develop/target-sdk>
  ([#203](https://github.com/rust-windowing/android-ndk-rs/pull/203))
- Allow manifest `package` property to be provided in `Cargo.toml`. ([#236](https://github.com/rust-windowing/android-ndk-rs/pull/236))
- Add `MAIN` intent filter in `from_subcommand` instead of relying on a custom serialization function in `rndk`. ([#241](https://github.com/rust-windowing/android-ndk-rs/pull/241))
- Export the sole `NativeActivity` (through `android:exported="true"`) to allow it to be started by default if targeting Android S or higher. ([#242](https://github.com/rust-windowing/android-ndk-rs/pull/242))
- `cargo-rapk` version can now be queried through `cargo apk version`. ([#218](https://github.com/rust-windowing/android-ndk-rs/pull/218))
- Environment variables from `.cargo/config.toml`'s `[env]` section are now propagated to the process environment. ([#249](https://github.com/rust-windowing/android-ndk-rs/pull/249))

# 0.8.2 (2021-11-22)

- Fixed the library name in case of multiple build artifacts in the Android manifest.
- Work around missing `libgcc` on NDK r23 beta 3 and above, by providing linker script that "redirects" to `libunwind`.
  See <https://github.com/rust-windowing/android-ndk-rs/issues/149> and <https://github.com/rust-lang/rust/pull/85806> for more details.

# 0.8.1 (2021-08-06)

- Updated to use [rndk 0.4.2](../rndk/CHANGELOG.md#042-2021-08-06)

# 0.8.0 (2021-07-06)

- Added `runtime_libs` path to android metadata for packaging extra dynamic libraries into the apk.

# 0.7.0 (2021-05-10)

- Added `cargo apk check`. Useful for compile-testing crates that contain C/C++ dependencies or
  target-specific conditional compilation, but do not provide a cdylib target.
- Added `apk_name` field to android metadata for APK file naming (defaults to Rust library name if unspecified).
  The application label is now no longer used for this purpose, and can contain a string resource ID from now on.

# 0.6.0 (2021-04-20)

- **Breaking:** uses `rndk`'s new (de)serialized `Manifest` struct to properly serialize a toml's `[package.metadata.android]` to an `AndroidManifest.xml`. The `[package.metadata.android]` now closely resembles the structure of [an android manifest file](https://developer.android.com/guide/topics/manifest/manifest-element). See [README](README.md) for an example of the new `[package.metadata.android]` structure and all manifest attributes that are currently supported.

# 0.5.6 (2020-11-25)

- Use `dunce::simplified` when extracting the manifest's assets and resource folder
- Updated to use [rndk 0.1.4](../rndk/CHANGELOG.md#014-2020-11-25)

# 0.5.5 (2020-11-21)

- Updated to use [rndk 0.1.3](../rndk/CHANGELOG.md#013-2020-11-21)

# 0.5.4 (2020-11-01)

- Added support for activity metadata entries.
- Fix glob member resolution in workspaces.

# 0.5.3 (2020-10-15)

- Fix `res` folder resolve.

# 0.5.2 (2020-09-15)

- Updated to use [rndk 0.1.2](../rndk/CHANGELOG.md#012-2020-09-15)

# 0.5.1 (2020-07-15)

- Updated to use [rndk 0.1.1](../rndk/CHANGELOG.md#011-2020-07-15)

# 0.5.0 (2020-04-22)

- Updated to use [rndk 0.1.0](../rndk/CHANGELOG.md#010-2020-04-22)
- First release in almost 3 years! 🎉
- **Breaking:** A ton of things changed!
