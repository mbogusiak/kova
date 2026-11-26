import org.jetbrains.kotlin.gradle.ExperimentalWasmDsl

plugins {
    kotlin("multiplatform") version "2.3.0"
}

group = "dev.kova.plugin"
version = "0.1.0"

kotlin {
    @OptIn(ExperimentalWasmDsl::class)
    wasmWasi {
        nodejs()
    }

    sourceSets {
        val wasmWasiMain by getting
        val wasmWasiTest by getting {
            dependencies {
                implementation(kotlin("test"))
            }
        }
    }
}
