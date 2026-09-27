#!/bin/bash

set -ex

# Create the logs up front so they are uploaded even when the run dies before
# logcat, which is what happens when the emulator itself never boots.
LOG="${HOME}/logcat.log"
APP_LOG="${HOME}/app.log"
: > "$LOG"
: > "$APP_LOG"

PKG=rust.example.hello_world
ACTIVITY="$PKG/android.app.NativeActivity"

# `sys.boot_completed` is set while system services are still coming up, so
# installing into a settling system can leave the activity unable to start.
# Wait for the package manager to be genuinely ready before doing anything.
adb wait-for-device
until [ "$(adb shell getprop sys.boot_completed | tr -d '\r')" = "1" ]; do
    echo "waiting for boot"
    sleep 2
done
adb shell wm dismiss-keyguard || true

# Make sure the package is removed since it may end up in the AVD cache. This causes
# INSTALL_FAILED_UPDATE_INCOMPATIBLE errors when the debug keystore is regenerated,
# as it is not stored/cached on the CI:
# https://github.com/rust-windowing/android-ndk-rs/blob/240389f1e281f582b84a8049e2afaa8677d901c2/rndk/src/ndk.rs#L308-L332
adb uninstall "$PKG" || true

if [ -z "$1" ];
then
    cargo rapk run -p ndk-examples --target x86_64-linux-android --example hello_world --no-logcat
else
    adb install -r "$1/hello_world.apk"
    adb shell am start -a android.intent.action.MAIN -n "$ACTIVITY"
fi

# The emulator is deliberately not killed here: the action tears it down
# itself, and doing it twice makes its teardown fail, which reports a passing
# test as a failure.

# Poll for the app's log line rather than sleeping a fixed amount: the example
# logs as soon as it starts, so this finishes sooner when it works and waits
# longer when the device is slow.
waited=0
while [ "$waited" -lt 120 ]; do
    if adb logcat -d -s hello_world:V | grep -q 'hello world'; then
        break
    fi
    waited=$((waited + 2))
    sleep 2
done

# `*:E` is needed to catch a Rust panic, but it is almost entirely emulator
# noise, so it only goes to the uploaded file. The step shows the app's own
# output, which is what actually has to be read when this fails.
adb logcat -d -s hello_world:V | tee "$APP_LOG"
adb logcat '*:E' hello_world:V -d > "$LOG"

if grep -q 'hello world' "$LOG";
then
    echo "App running"
else
    echo "::error::App not running; pidof: '$(adb shell pidof "$PKG" | tr -d '\r')'"
    # A native crash is recorded in the crash buffer, not the default one that
    # was dumped above, so without this a segfaulting `.so` looks like silence.
    echo "--- crash buffer ---"
    adb logcat -b crash -d || true
    echo "--- focused activity ---"
    adb shell dumpsys activity activities 2>/dev/null | grep -iE 'mResumedActivity|mFocusedApp' || true
    echo "--- recent activity errors ---"
    adb logcat -d -s ActivityManager:E AndroidRuntime:E linker:E 2>/dev/null | tail -40 || true
    exit 1
fi

ERROR_MSG=$(grep -e 'thread.*panicked at' "$LOG" | true)
if [ -z "${ERROR_MSG}" ];
then
    exit 0
else
    echo "::error::${ERROR_MSG}"
    exit 1
fi
