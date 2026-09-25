plugins {
    id("com.android.application")
}

android {
    namespace = "com.spacecorps.space3d.aitavern"
    // The same SDK levels as open-android-models (ML Kit GenAI needs a recent compileSdk).
    compileSdk {
        version = release(37) {
            minorApiLevel = 1
        }
    }

    defaultConfig {
        applicationId = "com.spacecorps.space3d.aitavern"
        minSdk = 26
        targetSdk = 37
        versionCode = 1
        versionName = "0.1.0"
        // Gemini Nano (AICore) devices are all arm64.
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Debug-signed so the release build installs for testing; sign with your own key to ship.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
}

dependencies {
    // OamJni (the JNI entry point space3d-ai's AndroidBackend calls), BridgeEngine and
    // GeminiNanoModel. Brings ML Kit GenAI Prompt and its manifest entries (AICore).
    implementation("com.spacecorps.oam:oam-mlkit:0.1.0-SNAPSHOT")
}

// The game's native library: cargo-ndk builds ../native (android_main + the game) for arm64-v8a
// into src/main/jniLibs, which AGP packages. `-PcargoProfile=dev` builds without --release;
// `-PrustToolchain=stable` picks the rustup toolchain that has the aarch64-linux-android target.
val cargoRelease = providers.gradleProperty("cargoProfile").map { it != "dev" }.orElse(true)
val rustToolchain = providers.gradleProperty("rustToolchain")
val nativeDir = rootProject.layout.projectDirectory.dir("native")
val jniLibsDir = layout.projectDirectory.dir("src/main/jniLibs")
val cargoNdk = tasks.register<Exec>("cargoNdk") {
    group = "build"
    description = "Builds the game's native library with cargo-ndk (needs cargo-ndk and the NDK)."
    workingDir = nativeDir.asFile
    val args = mutableListOf("cargo", "ndk", "-t", "arm64-v8a", "--platform", "26", "-o", jniLibsDir.asFile.absolutePath, "build")
    if (cargoRelease.get()) args += "--release"
    commandLine(args)
    rustToolchain.orNull?.let { environment("RUSTUP_TOOLCHAIN", it) }
}
tasks.named("preBuild") {
    dependsOn(cargoNdk)
}
