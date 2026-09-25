// Gradle host for a Space3d game on Android: NativeActivity + the game's Rust library (built
// with cargo-ndk) + open-android-models (OamJni, BridgeEngine, Gemini Nano through ML Kit).
// cargo-apk cannot merge AARs or their manifests, so games that use space3d-ai's
// AndroidBackend are packaged by Gradle. See docs/ai-android.md.
pluginManagement {
    repositories {
        google {
            content {
                includeGroupByRegex("com\\.android.*")
                includeGroupByRegex("com\\.google.*")
                includeGroupByRegex("androidx.*")
            }
        }
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode = RepositoriesMode.FAIL_ON_PROJECT_REPOS
    repositories {
        google()
        mavenCentral()
        // For com.spacecorps.oam artifacts published with `./gradlew publishToMavenLocal`.
        mavenLocal()
    }
}

rootProject.name = "ai-tavern-android"
include(":app")

// open-android-models from source: its modules publish as com.spacecorps.oam:<module>, so
// includeBuild substitutes them for the dependency in app/build.gradle.kts.
val oamDir = providers.gradleProperty("oamDir").orNull?.takeIf { it.isNotBlank() }
if (oamDir != null && file(oamDir).resolve("settings.gradle.kts").exists()) {
    includeBuild(oamDir)
}
