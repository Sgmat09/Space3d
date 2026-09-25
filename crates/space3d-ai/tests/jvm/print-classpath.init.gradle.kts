// Prints the JVM classpath tests/android_jvm.rs needs, from an open-android-models checkout:
//   ./gradlew -q --no-configuration-cache -I <this file> :oam-bridge:printOamJniClasspath
allprojects {
    if (path == ":oam-bridge") {
        tasks.register("printOamJniClasspath") {
            // Print only after the jar and oam-mlkit's classes exist: with org.gradle.parallel,
            // naming those tasks first on the command line does not order them.
            dependsOn("jar", ":oam-mlkit:bundleLibRuntimeToDirDebug")
            val runtime = configurations.getByName("runtimeClasspath")
            val jar = layout.buildDirectory.dir("libs")
            val mlkit = rootProject.layout.projectDirectory.dir("oam-mlkit/build/intermediates/runtime_library_classes_dir/debug/bundleLibRuntimeToDirDebug")
            doLast {
                val jars = jar.get().asFile.listFiles { f -> f.name.endsWith(".jar") && !f.name.contains("sources") }.orEmpty().toList()
                println((jars + runtime.files + mlkit.asFile).joinToString(java.io.File.pathSeparator))
            }
        }
    }
}
