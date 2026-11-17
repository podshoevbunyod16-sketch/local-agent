// Gradle-сборка (опционально)

Если позже понадобится AndroidX/Compose или публикация в Play, эту часть можно
переключить на Gradle + AGP без изменений в Rust-коде: `scripts/build-apk.sh`
делает то же самое вручную. Пример конфигурации ниже (AGP 8.7, JDK 17+).

## android/settings.gradle.kts

```kotlin
pluginManagement {
    repositories { google(); mavenCentral(); gradlePluginPortal() }
}
dependencyResolutionManagement {
    repositories { google(); mavenCentral() }
}
rootProject.name = "TurboLLM"
include(":app")
```

## android/build.gradle.kts

```kotlin
plugins {
    id("com.android.application") version "8.7.3" apply false
}
```

## android/app/build.gradle.kts

```kotlin
plugins { id("com.android.application") }

android {
    namespace = "ai.turbollm.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "ai.turbollm.app"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
    }

    sourceSets["main"].java.srcDir("src/main/java")
    // R.java генерируется aapt2; при Gradle-сборке удалите src/gen из sourceSets.

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    // Rust собирается cargo-ndk'ом до упаковки .so
    tasks.register<Exec>("cargoNdk") {
        workingDir = rootProject.projectDir.parentFile  // корень Cargo-workspace
        environment("ANDROID_NDK_HOME", System.getenv("ANDROID_NDK_HOME"))
        commandLine("cargo", "ndk", "-t", "arm64-v8a", "-t", "armeabi-v7a", "-t", "x86_64",
            "-o", "android/app/src/main/jniLibs", "--platform", "26",
            "build", "--release", "-p", "turbollm-core", "--lib")
    }
    tasks.named("preBuild") { dependsOn("cargoNdk") }
}
```

## Проверенный путь

`scripts/build-apk.sh` (aapt2 + javac + d8 + apksigner) собирает APK без
Gradle — именно он используется в CI, потому что не зависит от доступности
`services.gradle.org` и `google()` и занимает ~2–4 минуты.
