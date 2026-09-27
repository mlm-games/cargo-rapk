# 0.25.0 (2026-09-27)

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
