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
        check(wasmFile.exists()) {
            "Missing packaged WASM artifact: ${wasmFile.absolutePath}"
        }
        val wasmBytes = wasmFile.readBytes()
        check(wasmBytes.size > 8) {
            "Empty or truncated WASM artifact: ${wasmFile.absolutePath}"
        }
        check(
            wasmBytes[0] == 0x00.toByte() &&
                wasmBytes[1] == 0x61.toByte() &&
                wasmBytes[2] == 0x73.toByte() &&
                wasmBytes[3] == 0x6D.toByte(),
        ) {
            "Invalid WASM magic bytes in ${wasmFile.name}"
        }

        // Parse WASM section headers to verify all required ABI exports are present in the
        // production-optimized binary.
        var pos = 8
        fun readVarUInt(): Int {
            var result = 0
            var shift = 0
            while (true) {
                check(pos < wasmBytes.size) { "Unexpected EOF while reading WASM LEB128" }
                val b = wasmBytes[pos++].toInt() and 0xFF
                result = result or ((b and 0x7F) shl shift)
                if ((b and 0x80) == 0) return result
                shift += 7
            }
        }
        val exportedNames = mutableSetOf<String>()
        while (pos < wasmBytes.size) {
            val sectionId = wasmBytes[pos++].toInt() and 0xFF
            val sectionSize = readVarUInt()
            val sectionEnd = pos + sectionSize
            if (sectionId == 7) {
                val count = readVarUInt()
                repeat(count) {
                    val nameLen = readVarUInt()
                    val name = wasmBytes.copyOfRange(pos, pos + nameLen).decodeToString()
                    pos += nameLen
                    pos += 1 // export kind
                    readVarUInt() // export index
                    exportedNames += name
                }
            }
            pos = sectionEnd
        }
        val requiredExports = listOf(
            "memory",
            "kova_abi_version",
            "kova_alloc",
            "kova_free",
            "cabi_realloc",
            "kova_plugin_init",
            "kova_plugin_shutdown",
            "kova_plugin_metadata",
            "kova_plugin_render_ui",
            "kova_plugin_on_ui_event",
            "kova_plugin_on_command",
            "kova_plugin_on_event",
            "kova_plugin_last_error",
        )
        val missingExports = requiredExports.filterNot { it in exportedNames }
        check(missingExports.isEmpty()) {
            "Missing required ABI exports in ${wasmFile.name}: $missingExports"
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
