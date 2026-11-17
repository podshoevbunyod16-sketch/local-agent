#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# TurboLLM APK build WITHOUT Gradle.
#
# A normal Android build needs Gradle + AGP + a network connection to
# google()/mavenCentral(). This script uses only the SDK command line tools:
#
#   cargo-ndk (Rust -> libturbollm_core.so for 4 ABIs)
#   aapt2      (compile + link resources -> resources.arsc, R.java)
#   javac      (Java -> .class, against android.jar)
#   d8         (classes -> classes.dex)
#   zipalign   (align: uncompressed .so no longer needed, but good hygiene)
#   apksigner  (sign with a debug/CI keystore)
#
# Benefits: no Gradle download, no AGP version roulette, ~2-4 min in CI.
# Everything it needs is provided by the official GitHub runners:
#   ANDROID_SDK_ROOT, ANDROID_NDK_HOME, JAVA_HOME (17/21), Rust (dtolnay/rust-toolchain)
#
# Usage:
#   scripts/build-apk.sh                # all four ABIs, release, signed
#   ABIS="arm64-v8a x86_64" scripts/build-apk.sh
#   VARIANT=debug scripts/build-apk.sh  # debuggable + no LTO
# ---------------------------------------------------------------------------
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VARIANT="${VARIANT:-release}"
ABIS="${ABIS:-arm64-v8a armeabi-v7a x86_64 x86}"
OUT="${OUT:-$ROOT/build}"
PACKAGE="ai.turbollm.app"
APP_NAME="TurboLLM"
VERSION_NAME="${VERSION_NAME:-0.2.0}"
VERSION_CODE="${VERSION_CODE:-1}"

# ---- SDK / JDK discovery ---------------------------------------------------
: "${ANDROID_SDK_ROOT:=${ANDROID_HOME:-}}"
: "${ANDROID_NDK_HOME:=${ANDROID_NDK_ROOT:-}}"
if [ -z "$ANDROID_SDK_ROOT" ]; then
  echo "ANDROID_SDK_ROOT is not set" >&2; exit 1
fi
if [ -z "$ANDROID_NDK_HOME" ]; then
  if [ -d "$ANDROID_SDK_ROOT/ndk" ]; then
    ANDROID_NDK_HOME="$(ls -d "$ANDROID_SDK_ROOT"/ndk/* | sort -V | tail -1)"
  else
    echo "Android NDK not found" >&2; exit 1
  fi
fi
export ANDROID_NDK_HOME ANDROID_NDK="$ANDROID_NDK_HOME" ANDROID_SDK_ROOT ANDROID_HOME="$ANDROID_SDK_ROOT"

# cmake ships with the SDK; on minimal images it may not be on PATH.
if ! command -v cmake >/dev/null 2>&1 && [ -d "$ANDROID_SDK_ROOT/cmake" ]; then
  CMAKE_BIN="$(ls -d "$ANDROID_SDK_ROOT"/cmake/*/bin | sort -V | tail -1)"
  export PATH="$CMAKE_BIN:$PATH"
fi
command -v cmake >/dev/null 2>&1 || { echo "cmake not found (install SDK cmake;3.22.1)" >&2; exit 1; }

PLATFORM="${PLATFORM:-android-35}"
BUILD_TOOLS="${BUILD_TOOLS:-$(ls "$ANDROID_SDK_ROOT/build-tools" | sort -V | tail -1)}"
BT="$ANDROID_SDK_ROOT/build-tools/$BUILD_TOOLS"
ANDROID_JAR="$ANDROID_SDK_ROOT/platforms/$PLATFORM/android.jar"
MIN_SDK="${MIN_SDK:-26}"
TARGET_SDK="${TARGET_SDK:-35}"

for t in aapt2 d8 apksigner zipalign; do
  [ -x "$BT/$t" ] || { echo "missing $BT/$t" >&2; exit 1; }
done
[ -f "$ANDROID_JAR" ] || { echo "missing $ANDROID_JAR" >&2; exit 1; }

echo "== TurboLLM build =="
echo "variant:      $VARIANT"
echo "abis:         $ABIS"
echo "sdk platform: $PLATFORM ($ANDROID_JAR)"
echo "build-tools:  $BUILD_TOOLS"
echo "ndk:          $ANDROID_NDK_HOME"
echo "java:         $(java -version 2>&1 | head -1)"

rm -rf "$OUT"
mkdir -p "$OUT"/{rust,res,gen,java,dex,apk,lib}

# ---- 1. Rust core ----------------------------------------------------------
# `--no-default-features` drops the JNI crate for the host bench, but here we
# need it, so we build with default features (jni on).
NDK_TARGETS=()
for abi in $ABIS; do
  case "$abi" in
    arm64-v8a)   NDK_TARGETS+=("arm64-v8a") ;;
    armeabi-v7a) NDK_TARGETS+=("armeabi-v7a") ;;
    x86_64)      NDK_TARGETS+=("x86_64") ;;
    x86)         NDK_TARGETS+=("x86") ;;
    *) echo "unknown ABI $abi" >&2; exit 1 ;;
  esac
done

PROFILE_FLAG="--release"
[ "$VARIANT" = "debug" ] && PROFILE_FLAG=""

# ARM-микроархитектура: значение по умолчанию у llama-cpp-sys-2 — armv8-a, что
# не включает dotprod/i8mm-ядра ggml. Для распространняемого APK безопасный
# максимум — armv8.2-a+dotprod (есть на всех Cortex-A75+ и почти всех SoC
# 2019+); если нужно строго armv8-a, запустите с GGML_CPU_ARM_ARCH=armv8-a.
export GGML_CPU_ARM_ARCH="${GGML_CPU_ARM_ARCH:-armv8.2-a+dotprod}"

echo "== cargo ndk build ($PROFILE_FLAG) =="
if ! command -v cargo-ndk >/dev/null 2>&1; then
  cargo install cargo-ndk --locked
fi
# Make sure the Rust targets exist (GitHub runners ship rustup, so this is cheap).
for t in aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android; do
  rustup target list --installed 2>/dev/null | grep -qx "$t" || rustup target add "$t"
done
# shellcheck disable=SC2086
cargo ndk $(for abi in "${NDK_TARGETS[@]}"; do printf -- '-t %s ' "$abi"; done) \
  -o "$OUT/rust" --platform "$MIN_SDK" \
  build $PROFILE_FLAG -p turbollm-core --lib

# cargo-ndk writes $OUT/rust/<abi>/libturbollm_core.so
for abi in "${NDK_TARGETS[@]}"; do
  mkdir -p "$OUT/lib/$abi"
  cp "$OUT/rust/$abi/libturbollm_core.so" "$OUT/lib/$abi/"
  # libc++_shared.so is required because llama-cpp-2 is built with
  # android-shared-stdcxx (a single shared STL keeps the APK small).
  CXX_DIR="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib"
  case "$abi" in
    arm64-v8a)   SRC="$CXX_DIR/aarch64-linux-android/libc++_shared.so" ;;
    armeabi-v7a) SRC="$CXX_DIR/arm-linux-androideabi/libc++_shared.so" ;;
    x86_64)      SRC="$CXX_DIR/x86_64-linux-android/libc++_shared.so" ;;
    x86)         SRC="$CXX_DIR/i686-linux-android/libc++_shared.so" ;;
  esac
  [ -f "$SRC" ] && cp "$SRC" "$OUT/lib/$abi/"
done
du -sh "$OUT/lib"/*

# ---- 2. Resources ----------------------------------------------------------
MANIFEST_SRC="$ROOT/android/app/src/main/AndroidManifest.xml"
MANIFEST="$OUT/AndroidManifest.xml"
# aapt2 link substitutes versionCode/versionName from the manifest itself, so we
# only need to insert the numeric SDK versions if they are not already there.
python3 - "$MANIFEST_SRC" "$MANIFEST" "$MIN_SDK" "$TARGET_SDK" "$VERSION_CODE" "$VERSION_NAME" <<'PY'
import sys, re
src, dst, min_sdk, target_sdk, vcode, vname = sys.argv[1:7]
xml = open(src, encoding="utf-8").read()
xml = re.sub(r'android:versionCode="[^"]*"', 'android:versionCode="%s"' % vcode, xml)
xml = re.sub(r'android:versionName="[^"]*"', 'android:versionName="%s"' % vname, xml)
if 'android:minSdkVersion' not in xml:
    tag = '<uses-sdk android:minSdkVersion="%s" android:targetSdkVersion="%s" />' % (min_sdk, target_sdk)
    xml = xml.replace('<application', tag + '\n    <application', 1)
open(dst, "w", encoding="utf-8").write(xml)
PY

echo "== aapt2 compile =="
RES_DIR="$ROOT/android/app/src/main/res"
"$BT/aapt2" compile --dir "$RES_DIR" -o "$OUT/res/res.zip"

echo "== aapt2 link =="
"$BT/aapt2" link \
  -o "$OUT/apk/base-unsigned.apk" \
  -I "$ANDROID_JAR" \
  --manifest "$MANIFEST" \
  --java "$OUT/gen" \
  --min-sdk-version "$MIN_SDK" \
  --target-sdk-version "$TARGET_SDK" \
  --version-code "$VERSION_CODE" --version-name "$VERSION_NAME" \
  --auto-add-overlay \
  "$OUT/res/res.zip"

ASSETS_DIR="$ROOT/android/app/src/main/assets"
if [ -d "$ASSETS_DIR" ] && [ -n "$(ls -A "$ASSETS_DIR" 2>/dev/null)" ]; then
  ( cd "$ASSETS_DIR" && zip -q -r "$OUT/apk/base-unsigned.apk" . )
fi

# ---- 3. Java -> class -> dex ----------------------------------------------
echo "== javac =="
# The hand-written R.java in src/gen is replaced by aapt2's generated one.
gen_src="$OUT/gen"
find "$gen_src" -name 'R.java'
find "$ROOT/android/app/src/main/java" -name '*.java' > "$OUT/java/sources.txt"
find "$gen_src" -name 'R.java' >> "$OUT/java/sources.txt"
# NOTE: android.jar goes on -classpath, not -bootclasspath. The android.jar stubs
# intentionally lack java.lang.invoke.LambdaMetafactory#metafactory, so using it
# as the boot class path makes javac reject lambdas and method references. With
# the JDK boot class path javac emits plain invokedynamic and d8 desugars it
# (min-api 26 supports lambdas natively).
javac -source 8 -target 8 -encoding UTF-8 -Xlint:-options \
  -classpath "$ANDROID_JAR" \
  -d "$OUT/java/classes" @"$OUT/java/sources.txt" || {
    echo "javac failed" >&2; exit 1; }

echo "== d8 =="
find "$OUT/java/classes" -name '*.class' > "$OUT/dex/classes.txt"
"$BT/d8" --release --min-api "$MIN_SDK" --lib "$ANDROID_JAR" \
  --output "$OUT/dex" $(cat "$OUT/dex/classes.txt")

# ---- 4. Package ------------------------------------------------------------
echo "== package =="
PKG_DIR="$OUT/apk/stage"
mkdir -p "$PKG_DIR"
( cd "$PKG_DIR" && unzip -q -o "$OUT/apk/base-unsigned.apk" )
cp "$OUT/dex/classes.dex" "$PKG_DIR/"
for abi in "${NDK_TARGETS[@]}"; do
  mkdir -p "$PKG_DIR/lib/$abi"
  cp "$OUT/lib/$abi/"*.so "$PKG_DIR/lib/$abi/"
done
( cd "$PKG_DIR" && zip -q -r -9 "$OUT/apk/app-unaligned.apk" . -x '.*' )
"$BT/zipalign" -p -f 4 "$OUT/apk/app-unaligned.apk" "$OUT/apk/app-aligned.apk"

# ---- 5. Sign ---------------------------------------------------------------
KEYSTORE="${KEYSTORE:-$ROOT/android/keystore/debug.keystore}"
KS_PASS="${KS_PASS:-turbollm}"
KEY_ALIAS="${KEY_ALIAS:-turbollm}"
if [ ! -f "$KEYSTORE" ]; then
  echo "== generating debug keystore =="
  mkdir -p "$(dirname "$KEYSTORE")"
  keytool -genkeypair -v -keystore "$KEYSTORE" -storepass "$KS_PASS" -keypass "$KS_PASS" \
    -alias "$KEY_ALIAS" -keyalg RSA -keysize 2048 -validity 10000 \
    -dname "CN=TurboLLM, OU=CI, O=TurboLLM, L=Dushanbe, C=TJ" >/dev/null 2>&1
fi

# Необязательный второй пароль (PKCS12 обычно использует тот же).
KEY_PASS="${KEY_PASS:-$KS_PASS}"
APK_NAME="${APP_NAME}-${VERSION_NAME}-${VARIANT}.apk"
"$BT/apksigner" sign \
  --ks "$KEYSTORE" --ks-pass "pass:$KS_PASS" --key-pass "pass:$KEY_PASS" \
  --ks-key-alias "$KEY_ALIAS" \
  --min-sdk-version "$MIN_SDK" \
  --out "$OUT/$APK_NAME" "$OUT/apk/app-aligned.apk"
"$BT/apksigner" verify --print-certs "$OUT/$APK_NAME" | head -5

echo
echo "✅ $OUT/$APK_NAME"
ls -lh "$OUT/$APK_NAME"
unzip -l "$OUT/$APK_NAME" | grep -E "lib/|classes.dex" || true
