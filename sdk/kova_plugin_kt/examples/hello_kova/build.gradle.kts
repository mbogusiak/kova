import org.jetbrains.kotlin.gradle.ExperimentalWasmDsl

plugins {
    kotlin("multiplatform")
}

group = "dev.kova.examples"
version = "0.1.0"

kotlin {
    @OptIn(ExperimentalWasmDsl::class)
    wasmWasi {
        binaries.executable()
        nodejs()
    }

    sourceSets {
        val wasmWasiMain by getting {
            dependencies {
                implementation(project(":"))
            }
        }
        val wasmWasiTest by getting {
            dependencies {
                implementation(kotlin("test"))
            }
        }
    }
}

val packagePlugin by tasks.registering(Copy::class) {
    group = "kova"
    description = "Packages hello-kova.wasm and plugin.toml for ~/.local/share/kova/plugins/hello-kova/"
    dependsOn("compileProductionExecutableKotlinWasmWasiOptimize")

    val wasmOutputDir = layout.buildDirectory.dir("compileSync/wasmWasi/main/productionExecutable/optimized")
    from(wasmOutputDir) {
        include("*.wasm")
        rename { "hello-kova.wasm" }
    }
    from(layout.projectDirectory.file("plugin.toml"))
    into(layout.buildDirectory.dir("kova-plugin-package/hello-kova"))
}

val installKovaPlugin by tasks.registering(Copy::class) {
    group = "kova"
    description = "Installs the packaged hello-kova plugin into ~/.local/share/kova/plugins/hello-kova/"
    dependsOn(packagePlugin)
    from(layout.buildDirectory.dir("kova-plugin-package/hello-kova"))
    into(file("${System.getProperty("user.home")}/.local/share/kova/plugins/hello-kova"))
}

val verifyPluginPackage by tasks.registering {
    group = "verification"
    description = "Validates the packaged hello-kova.wasm binary header and plugin.toml manifest"
    dependsOn(packagePlugin)

    val packageDir = layout.buildDirectory.dir("kova-plugin-package/hello-kova")
    inputs.dir(packageDir)

    doLast {
        val dir = packageDir.get().asFile
        val wasmFile = dir.resolve("hello-kova.wasm")
        val manifestFile = dir.resolve("plugin.toml")
        check(wasmFile.exists() && wasmFile.length() > 8) {
            "Missing or empty packaged WASM artifact: ${wasmFile.absolutePath}"
        }
        val header = wasmFile.inputStream().use { stream ->
            ByteArray(4).also { check(stream.read(it) == 4) { "Short WASM header" } }
        }
        check(
            header[0] == 0x00.toByte() &&
                header[1] == 0x61.toByte() &&
                header[2] == 0x73.toByte() &&
                header[3] == 0x6D.toByte(),
        ) {
            "Invalid WASM magic bytes in ${wasmFile.name}"
        }
        check(manifestFile.exists()) {
            "Missing plugin.toml in ${dir.absolutePath}"
        }
        val manifestText = manifestFile.readText()
        check(manifestText.contains("entrypoint = \"hello-kova.wasm\"")) {
            "plugin.toml must declare entrypoint = \"hello-kova.wasm\""
        }
    }
}

tasks.named("assemble") {
    dependsOn(packagePlugin)
}

tasks.named("check") {
    dependsOn(verifyPluginPackage)
}
