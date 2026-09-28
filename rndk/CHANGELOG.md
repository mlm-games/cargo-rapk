# 0.25.0 (2026-09-28)
- Add `maven::Role` and `maven::parse_entry`, letting an `android_libs` entry carry `!platform` or `!library`, and add `declared_platforms` and `declared_libraries` to the resolver. Gradle takes a platform off the declaration and applies no `<dependencyManagement>` to a module named as an ordinary dependency; `<packaging>` cannot say which was meant, so the entry can.
- **Fix:** Key a relocated module by the coordinates a dependency names, hang the target off it as an edge, and ship nothing for the original. `GradlePomModuleDescriptorBuilder.addDependencyForRelocation` adds the target as a compile-scoped dependency and `setRelocated(true)` costs the module its own artifacts, so the graph mediates on the original and the target resolves as a module in its own right. Re-keying onto the target meant a module relocated at one version and not another never had its two claims compared.
- **Fix:** Refuse a relocation to another version of the same coordinates, which names a module that publishes nothing under its own name, rather than shipping the target's bytes under the old name or silently shipping nothing.
- **Fix:** Take a `pom`-packaged module's own jar when the repository publishes one, and treat its absence as not-published rather than a failure, as `metadata.optionalArtifact` models it. The AAR-then-jar fallback reported a missing jar as an error, which failed every closure holding a BOM.
- **Fix:** Hand a repository's token and password to `curl` through `--config -`, on standard input, rather than as `-H` and `-u` arguments that any process on the machine can read from `/proc/<pid>/cmdline`.
- **Fix:** Remove the variant-selection "matches the most attributes" step and put the extra-attribute steps in Gradle's order, preferring a variant that declares an extra attribute before one that lacks it. Verified against `MultipleCandidateMatcher.disambiguateCompatibleCandidates`.
- **Fix:** Reject `org.gradle.usage=java-api` on a runtime classpath, and accept a `-jars`/`-classes`/`-resources` usage as its bare form. Verified against `JavaEcosystemSupport.UsageCompatibilityRules` and `UsageCompatibilityHandler`.
- **Fix:** Read an attribute value that is a boolean or a number, adding `gmm::Value`. Verified against the specification and against `androidx.annotation:annotation:1.3.0`.
- **Fix:** Settle `org.gradle.jvm.version` in Gradle's disambiguation precedence, ahead of `org.jetbrains.kotlin.platform.type`.
- **Fix:** Enforce a GMM `rejects` list, adding `Requirement::Rejecting` and `Requirement::rejecting`. Verified against `DefaultResolvedVersionConstraint.accepts` and `RepositoryChainDependencyToComponentIdResolver`.
- **Fix:** Treat `<scope>import</scope>` alone as an import, merge `dependencyManagement` as `local > import > parent`, and resolve two imports of one module to the first declared. Verified against `PomReader.resolveDependencyMgt` and `GradlePomModuleDescriptorParser.parseImportedDependencyMgts`.
- **Fix:** Exclude a platform's import entries from the versions it coordinates, as `GradlePomModuleDescriptorParser.addDependencies` does.
- **Fix:** Report a capability conflict only when two resolved artifacts provide the same one and agree on a version, and resolve a version disagreement to the highest. Verified against `UpgradeCapabilityResolver` and `LastCandidateCapabilityResolver`.
- Add `maven::unserved`, for a group no configured repository serves.
- Skip a `<type>test-jar</type>` dependency, which is a test fixture rather than a library and is absent from the artifact a fetch would look for.
- Do not fetch a `.sha1` for an artifact a cached file already came from, so a repository publishing no sidecar costs one download rather than one per build.
- **Fix:** Read `java.specification.version` from `java -XshowSettings:properties`, which prints spaces around the `=`. Read wrong, `Ndk::java_version` was always `None` and `gmm::target_jvm_version` fell back to Java 8.
- **Fix:** Exclude a root `module-info.class` and a `META-INF/versions/` overlay from `duplicate_classes`; neither is a class `d8` dexes.
- **Fix:** Give every `Repository` field but `url` a serde default, so a minimal entry does not fail the read.
- **Fix:** Leave an unterminated `${` in a repository field as written, rather than emitting the preceding text twice.
- **Fix:** Return no repository for a group none is configured to serve, instead of every one of them, and add `maven::unserved` for the message.
- **Fix:** Expand `${NAME}` in the environment-variable repository forms, and read them as either a bare array or a `[[repository]]` table.
- **Fix:** Reuse a cached file from a repository that publishes no `.sha1` rather than re-downloading it.
- **Fix:** Read `org.gradle.jvm.version` written as `1.8`, which parses as nothing and so looked like no requirement at all.
- **Fix:** Name the artifact directory in a `duplicate_classes` report, not the version directory.
- **Fix:** Resolve a `pom`-packaged module's own dependencies, which treating it purely as a platform dropped.
- **Fix:** Apply a platform whose versions are only `dependencyConstraints` in its `.module`, and treat a module as a platform only when nothing else it offers is a usable library.
- **Fix:** Key a recorded `artifactSelector` by the relocation target, so a relocated module keeps its classifier. The target is now a node of its own, reached as an edge from the module a dependency names, so the selector is recorded under the name the graph actually keys the artifact by.
- **Fix:** Apply a `requestedCapabilities` requirement, which was parsed and never checked.
- **Fix:** Cache the negative result of a metadata lookup, so a module publishing no `.module` is not re-probed for on every call.
- **Fix:** Find a platform variant directly, since `select` requests `org.gradle.category=library` and rejects one by design.

- Add `maven::Repository` and `maven::set_repositories`, and search every repository in order rather than deriving one from the group prefix. Add `maven::repo_bases` and `maven::artifact_url_classified`, and a `classifier` on `Resolved`, `ResolvedLib` and the fetch, for an artifact published under one.
- Add `maven::duplicate_classes`, reporting each class two archives both define and which archives those are. `META-INF/` is excluded, being JPMS descriptors rather than classes to dex.
- Read a library's `uses-sdk` `minSdkVersion` as `ResolvedLib::min_sdk_version`.
- Add `gmm::Capability` and `gmm::ArtifactSelector`, read from a variant's `capabilities` and a dependency's `requestedCapabilities` and `thirdPartyCompatibility`. Reject two resolved modules providing one capability.
- Add `gmm::Module::select_for_jvm` and honour `org.gradle.jvm.version` when choosing a variant; `gmm::target_jvm_version` reports the JDK the build will use, overridable with `CARGO_RAPK_JVM_VERSION`.
- Verify a download against a `.sha1` sidecar when the repository publishes one and report it when it does not, rather than failing. A sidecar that disagrees still fails.
- **Breaking:** `manifest::parse_library_manifest` returns a `LibraryManifest`, which carries the `AndroidManifest` and an `unmodelled` list of what a library declared and could not be carried over. Previously anything dropped was invisible.
- **Breaking:** Add `Permission::protection_level`, `Application::app_component_factory`, `Application::uses_library` and the `UsesLibrary` type, `Activity::theme`, `Activity::{exclude_from_recents, fits_system_windows, state_not_needed}`, `Service::visible_to_instant_apps`, `Provider::direct_boot_aware`, `IntentFilter::priority`, and `IntentFilterData::{path_suffix, path_advanced_pattern}`.
- **Breaking:** `AndroidManifest::merge_library` takes the library's `AndroidManifest` as before, but a library's `appComponentFactory` is now carried when the app has none of its own.
- Add `Ndk::java_version`, reading `java.specification.version` from the JDK.
- Add `NdkError::{DuplicateClasses, LibraryMinSdkTooHigh}`.

- **Breaking:** Add transitive Maven resolution for `android_libs`, following POMs and Gradle Module Metadata, with newest-wins mediation and a warning when the selected version falls outside a hard range. Add the `pom`, `gmm`, `range` and `version` modules, and `maven::{ensure_lib, Coordinates, ResolvedLib}`.
- **Breaking:** Add `AndroidManifest::{permission, grant_uri_permission}`, `tools_node` on `Activity`, `Service`, `Receiver` and `Provider`, and `enabled` on `Activity` and `Service`. `Receiver::enabled` and `Provider::enabled` widen from `Option<bool>` to `Option<Enabled>`. Downstream struct literals of these public types need updating.
- **Breaking:** Read a library's `AndroidManifest.xml` with an event parser, `manifest::parse_library_manifest`, rather than the `quick-xml` deserializer, which cannot read the `android:`-prefixed attributes these types are declared with. `merge_library` now has components to merge.
- **Breaking:** Build the APK with `aapt2` instead of `aapt` v1, so `resources.arsc` differs from any earlier build.
- Add `Enabled`, which keeps a resource reference in `android:enabled` instead of collapsing it to a boolean, with serialization as a plain attribute value and deserialization from either a boolean or a string.
- Read library `activity`, `service`, `uses-feature` (including `android:glEsVersion`) and `grant-uri-permission` elements, honour `tools:node` on every component, and read self-closing components, which were skipped.
- Read a cached library manifest from its archive rather than a line-encoded sidecar file, which truncated it at the first newline and so recovered nothing.
- Extract a library's `res/` to the path under `res/` rather than a nested `res/res/`, which left the tree `aapt2` compiles empty.
- Render a version range in Maven syntax rather than a Rust debug dump, so a range in a diagnostic reads `[1.0]` and not a struct literal.
- Verify a downloaded POM and Gradle Module Metadata against their `.sha1` sidecar, as an artifact already was, and fail closed when it is absent.

# 0.24.0 (2026-09-27)

- Link libraries with 16KiB alignment for NDK versions older than r28, so that APKs remain loadable on Android 15+ devices that run with 16KiB pages. ([#76](https://github.com/rust-mobile/cargo-apk/pull/76))
- Resolve `build.rustflags` and `target.<triple>.rustflags` from the Cargo configuration hierarchy and merge them into the exported `CARGO_ENCODED_RUSTFLAGS`, which otherwise shadows them. Builds that configured `build.rustflags` but relied on `cargo rapk` dropping them now receive those flags, as plain `cargo build` does; set `RUSTFLAGS` to keep overriding the configuration. ([#22](https://github.com/rust-mobile/cargo-apk/issues/22))
- Fall back to an installed minor API level (such as `android-36.1`) when no exact `platforms/android-NN` directory exists. ([#75](https://github.com/rust-mobile/cargo-apk/pull/75))
- Distinguish "no SDK platforms installed" from "no installed platform is supported by this NDK", and name the platforms found and supported in the error. ([#79](https://github.com/rust-mobile/cargo-apk/pull/79))
- Add `uses-native-library` and `profileable` elements, a `receiver` element, and the `android:installLocation`, `android:requestLegacyExternalStorage` and `android:allowNativeHeapPointerTagging` attributes on `Application`. ([#87](https://github.com/rust-mobile/cargo-apk/pull/87), [#85](https://github.com/rust-mobile/cargo-apk/pull/85), [#58](https://github.com/rust-mobile/cargo-apk/pull/58), [#81](https://github.com/rust-mobile/cargo-apk/issues/81))
- Look up the uid under the device's current user, and launch into it, so that multi-user devices with a work profile no longer report another user's uid. ([#57](https://github.com/rust-mobile/cargo-apk/pull/57))
- Do not copy permission bits when adding a library to the APK, so that a rebuild does not fail against a read-only source such as a Nix store SDK. ([#21](https://github.com/rust-mobile/cargo-apk/pull/21))
- Report a non-UTF-8 path or command output as an error instead of panicking, and a `SOURCE_DATE_EPOCH` past 2107 as an error instead of silently stamping entries with the DOS epoch. Timestamps before 1980, including the conventional `SOURCE_DATE_EPOCH=0`, now clamp to the DOS epoch as they always did.

# 0.10.0 (2023-11-30)

- Add `android:extractNativeLibs`, `android:usesCleartextTraffic` attributes to the manifest's `Application` element, and `android:alwaysRetainTaskState` to the `Activity` element. ([#15](https://github.com/mlm-games/cargo-rapk/pull/15))
- Enable building from `android` host. ([#29](https://github.com/mlm-games/cargo-rapk/pull/29))
- Use app `uid` instead of `pid` to limit `logcat` output to the current app. ([#33](https://github.com/mlm-games/cargo-rapk/pull/33))

# 0.9.0 (2022-11-23)

- Add `ndk::DEFAULT_DEV_KEYSTORE_PASSWORD` and make `apk::ApkConfig::apk` public. ([#358](https://github.com/rust-windowing/android-ndk-rs/pull/358))
- `RUSTFLAGS` is now considered if `CARGO_ENCODED_RUSTFLAGS` is not present allowing `cargo rapk build` to not break users' builds if they depend on `RUSTFLAGS` being set prior to the build,
  as `CARGO_ENCODED_RUSTFLAGS` set by `rndk` before invoking `cargo` will take precedence over [all other sources of build flags](https://doc.rust-lang.org/cargo/reference/config.html#buildrustflags). ([#357](https://github.com/rust-windowing/android-ndk-rs/pull/357))
- Add `ApkConfig::strip`, allowing a user to specify how they want debug symbols treated after cargo has finished building, but before the shared object is copied into the APK. ([#356](https://github.com/rust-windowing/android-ndk-rs/pull/356))

(0.8.1, released on 2022-10-14, was yanked due to violating semver.)

- **Breaking:** Provide `reverse_port_forwarding()` to set up `adb reverse` ([#348](https://github.com/rust-windowing/android-ndk-rs/pull/348))

# 0.8.0 (2022-09-12)

- **Breaking:** Postpone APK library packaging until before zip alignment, to deduplicate possibly overlapping entries. ([#333](https://github.com/rust-windowing/android-ndk-rs/pull/333))
- Add `adb` device serial parameter to `detect_abi()` and `Apk::{install,start}()`. ([#329](https://github.com/rust-windowing/android-ndk-rs/pull/329))
- Fix missing `.exe` extension for `adb` on Windows inside `detect_abi()`. ([#339](https://github.com/rust-windowing/android-ndk-rs/pull/339))
- `start()` now returns the PID of the started app process (useful for passing to `adb logcat --pid`). ([#331](https://github.com/rust-windowing/android-ndk-rs/pull/331))
- Inherit `ndk_gdb()` function from `cargo-rapk` with the appropriate script extension across platforms. ([#330](https://github.com/rust-windowing/android-ndk-rs/pull/330), [#258](https://github.com/rust-windowing/android-ndk-rs/pull/258))
- Provide `adb` path to `ndk-gdb`, allowing it to run without `adb` in `PATH`. ([#343](https://github.com/rust-windowing/android-ndk-rs/pull/343))
- Remove quotes from `Android.mk` to fix `ndk-gdb` on Windows. ([#344](https://github.com/rust-windowing/android-ndk-rs/pull/344))
- Launch Android activity through `ndk-gdb` to block app start until the debugger is attached. ([#345](https://github.com/rust-windowing/android-ndk-rs/pull/345))
- Consider `ANDROID_SDK_ROOT` as deprecated instead of `ANDROID_HOME`. ([#346](https://github.com/rust-windowing/android-ndk-rs/pull/346))
- **Breaking:** Rename `fn android_dir()` to `fn android_user_home()` and seed with `ANDROID_SDK_HOME` or `ANDROID_USER_HOME`. ([#347](https://github.com/rust-windowing/android-ndk-rs/pull/347))

# 0.7.0 (2022-07-05)

- Fix NDK r23 `-lgcc` workaround for target directories containing spaces. ([#298](https://github.com/rust-windowing/android-ndk-rs/pull/298))
- Invoke `clang` directly instead of through the NDK's wrapper scripts. ([#306](https://github.com/rust-windowing/android-ndk-rs/pull/306))
- **Breaking:** Rename `Activity::intent_filters` back to `Activity::intent_filter`. ([#305](https://github.com/rust-windowing/android-ndk-rs/pull/305))

# 0.6.0 (2022-06-11)

- **Breaking:** Provide NDK r23 `-lgcc` workaround in `cargo_ndk()` function, now requiring `target_dir` as argument. ([#286](https://github.com/rust-windowing/android-ndk-rs/pull/286))
- **Breaking:** Add `disable_aapt_compression` field to `ApkConfig` to disable `aapt` compression. ([#283](https://github.com/rust-windowing/android-ndk-rs/pull/283))

# 0.5.0 (2022-05-07)

- **Breaking:** Default `target_sdk_version` to `30` or lower (instead of the highest supported SDK version by the detected NDK toolchain)
  for more consistent interaction with Android backwards compatibility handling and its increasingly strict usage rules:
  <https://developer.android.com/distribute/best-practices/develop/target-sdk>
- **Breaking:** Remove default insertion of `MAIN` intent filter through a custom serialization function, this is better filled in by
  the default setup in `cargo-rapk`. ([#241](https://github.com/rust-windowing/android-ndk-rs/pull/241))
- Add `android:exported` attribute to the manifest's `Activity` element. ([#242](https://github.com/rust-windowing/android-ndk-rs/pull/242))
- Add `android:sharedUserId` attribute to the manifest's top-level `manifest` element. ([#252](https://github.com/rust-windowing/android-ndk-rs/pull/252))
- Add `queries` element to the manifest's top-level `manifest` element. ([#259](https://github.com/rust-windowing/android-ndk-rs/pull/259))

# 0.4.3 (2021-11-22)

- Provide NDK `build_tag` version from `source.properties` in the NDK root.

# 0.4.2 (2021-08-06)

- Pass UNIX path separators to `aapt` on non-UNIX systems, ensuring the resulting separator is compatible with the target device instead of the host platform.

# 0.4.1 (2021-08-02)

- Only the highest platform supported by the NDK is now selected as default platform.

# 0.4.0 (2021-07-06)

- Added `add_runtime_libs` function for including extra dynamic libraries in the APK.

# 0.3.0 (2021-05-10)

- New `ApkConfig` field `apk_name` is now used for APK file naming, instead of the application label.
- Renamed `cargo_rapk` utility to `cargo_ndk`.

# 0.2.0 (2021-04-20)

- **Breaking:** refactored `Manifest` into a proper (de)serialization struct. `Manifest` now closely matches [an android manifest file](https://developer.android.com/guide/topics/manifest/manifest-element).
- **Breaking:** removed `Config` in favor of using the new `Manifest` struct directly. Instead of using `Config::from_config` to create a `Manifest`, now you instantiate `Manifest` directly using, almost all, the same values.

# 0.1.4 (2020-11-25)

- On Windows, fixed UNC path handling for resource folder.

# 0.1.3 (2020-11-21)

- `android:launchMode` is configurable.

# 0.1.2 (2020-09-15)

- `android:label` is configurable.
- Library search paths are much more intelligent.
- `android:screenOrientation` is configurable.

# 0.1.1 (2020-07-15)

- Added support for custom intent filters.
- On Windows, fixed UNC path handling.
- Fixed toolchain path handling when the NDK installation has no host arch suffix on its prebuilt LLVM directories.

# 0.1.0 (2020-04-22)

- Initial release! 🎉
