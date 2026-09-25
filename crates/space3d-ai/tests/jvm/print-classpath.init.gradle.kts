// Prints the JVM classpath tests/android_jvm.rs needs, from an open-android-models checkout:
//   ./gradlew -q --no-configuration-cache -I <this file> :oam-bridge:jar :oam-mlkit:bundleLibRuntimeToDirDebug :oam-bridge:printOamJniClasspath
allprojects {
    if (path == ":oam-bridge") {
        tasks.register("printOamJniClasspath") {
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
