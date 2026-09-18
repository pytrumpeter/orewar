# Points this shell at the Android toolchain, for building the APK.
#
# Dot-source it before any Android build: `. .\android-env.ps1`. The JDK, SDK
# and NDK are all installed under the user profile, so nothing here depends on
# machine-wide state, and nothing here changes how the desktop build behaves.

$env:JAVA_HOME = "C:\Program Files (Arm)\Microsoft\jdk-17.0.10.7-hotspot"
$env:ANDROID_HOME = "$env:LOCALAPPDATA\Android\Sdk"
$env:ANDROID_NDK_HOME = "$env:ANDROID_HOME\ndk\28.2.13676358"
$env:ANDROID_NDK_ROOT = $env:ANDROID_NDK_HOME

# Google ships no ARM64 build of the NDK for Windows, so on this machine the
# x86_64 toolchain runs under emulation. It works; it is just slow.
$ndkBin = "$env:ANDROID_NDK_HOME\toolchains\llvm\prebuilt\windows-x86_64\bin"

# The API level the C compiler targets. Must not be above `min_sdk_version` in
# the Android crate's manifest, or the binary asks the device for symbols an
# older Android does not have.
$api = 28

# `cc` looks up the compiler by these, and the linker is set per target rather
# than in `.cargo/config.toml` so that nothing about the desktop build changes.
$env:CC_aarch64_linux_android = "$ndkBin\aarch64-linux-android$api-clang.cmd"
$env:CXX_aarch64_linux_android = "$ndkBin\aarch64-linux-android$api-clang++.cmd"
$env:AR_aarch64_linux_android = "$ndkBin\llvm-ar.exe"
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = $env:CC_aarch64_linux_android

$env:PATH = "$env:JAVA_HOME\bin;$env:ANDROID_HOME\platform-tools;$ndkBin;$env:PATH"

Write-Host "Android toolchain ready: NDK $(Split-Path $env:ANDROID_NDK_HOME -Leaf), API $api"

# One thing worth knowing about the SDK installer: `android sdk install` exits 9
# on success as well as on failure, so an exit code from it means nothing. Check
# whether the directory appeared instead.
