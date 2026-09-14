import org.gradle.nativeplatform.platform.internal.DefaultNativePlatform
import org.jetbrains.kotlin.gradle.tasks.KotlinCompile

plugins {
    alias(libs.plugins.kotlinMultiplatform)
    alias(libs.plugins.composeMultiplatform)
    alias(libs.plugins.composeCompiler)
}

val os = DefaultNativePlatform.getCurrentOperatingSystem()
val arch = DefaultNativePlatform.getCurrentArchitecture()

val libName = "camera_engine"

val jnaPlatformFolder = when {
    os.isWindows -> if (arch.isArm64) "win32-aarch64" else "win32-x86-64"
    os.isMacOsX -> if (arch.isArm64) "darwin-aarch64" else "darwin-x86-64"
    os.isLinux -> if (arch.isArm64) "linux-aarch64" else "linux-x86-64"
    else -> "generic"
}

val nativeLibFilename = when {
    os.isWindows -> "$libName.dll"
    os.isMacOsX -> "lib$libName.dylib"
    else -> "lib$libName.so"
}

val rustDir = file("${rootDir}/rust") // Caminho para a pasta do projeto Rust
val rustOutputDir = file("${rustDir}/target/release")
val rustOutputLib = file("${rustDir}/target/release/$nativeLibFilename")
val generatedKotlinDir = layout.buildDirectory.dir("generated/uniffi/kotlin")

// Task for compiling Rust code using Cargo
val buildRustRelease by tasks.registering(Exec::class) {
    group = "rust"
    description = "Compile Rust code in release mode to the current platform"

    workingDir = rustDir

    commandLine("cargo", "build", "--release")

    // Define inputs and outputs for Gradle reuse cache if nothing changed
    inputs.dir("${rustDir}/camera_engine/src")
    inputs.file("${rustDir}/camera_engine/Cargo.toml")
    inputs.file("${rustDir}/Cargo.toml")
    outputs.file(rustOutputLib)
}

// Generate Kotlin bindings using UniFFI CLI from Cargo
val generateUniFFIBindings by tasks.registering(Exec::class) {
    group = "rust"
    description = "Generate Kotlins bindings from UniFFI's scaffolding"
    dependsOn(buildRustRelease)
    workingDir = rustDir

    // Run the generated uniffi
    commandLine(
        "cargo", "run",
        "-p", libName,
        "--bin", "uniffi-bindgen",
        "--",
        "generate",
        "--library", rustOutputLib.absolutePath,
        "--language", "kotlin",
        "--out-dir", generatedKotlinDir.get().asFile.absolutePath
    )

    inputs.file(rustOutputLib)
    outputs.dir(generatedKotlinDir)
}

// Ensuring that native resources will be processed before the execution
tasks.withType<ProcessResources>().configureEach {
    dependsOn(generateUniFFIBindings)
}

kotlin {
    jvm()

    sourceSets {
        val jvmMain by getting {
            kotlin.srcDir(generatedKotlinDir)
            resources.srcDir(rustOutputDir)
        }

        jvmMain.dependencies {
            implementation(compose.desktop.currentOs)
            implementation("net.java.dev.jna:jna:5.14.0")
        }

        commonMain.dependencies {
            implementation(libs.compose.runtime)
            implementation(libs.compose.foundation)
            implementation(libs.compose.material3)
            implementation(libs.compose.ui)
            implementation(libs.compose.components.resources)
            implementation(libs.compose.uiToolingPreview)
            implementation(libs.androidx.lifecycle.viewmodelCompose)
            implementation(libs.androidx.lifecycle.runtimeCompose)
            implementation(project.dependencies.platform(libs.koin.bom))
            implementation(libs.koin.compose)
            implementation(libs.koin.compose.viewmodel)
            implementation(libs.koin.compose.viewmodel.navigation)
        }
        commonTest.dependencies {
            implementation(libs.kotlin.test)
        }
    }
}

// Ensures that Kotlin wait for the generated *.kt files before compiling
tasks.withType<KotlinCompile>().configureEach {
    dependsOn(generateUniFFIBindings)
}