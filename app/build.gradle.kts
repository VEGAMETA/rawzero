plugins {
    id("com.android.application")
}

android {
    namespace = "dev.vegameta.rawzero"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.vegameta.rawzero"
        minSdk = 30
        targetSdk = 36
        versionCode = 1
        versionName = "0.1"
        ndk { abiFilters += "arm64-v8a" }
    }

    // Camera allow-list experiments: the same APK under package names that vendor
    // cameraservers are known to let see auxiliary lenses. Personal sideloading only;
    // a flavor cannot be installed while the real package with that name is present.
    flavorDimensions += "pkg"
    productFlavors {
        create("rawzero") { dimension = "pkg" }
        create("scan3d") { dimension = "pkg"; applicationId = "com.samsung.android.scan3d" }
        create("ruler") { dimension = "pkg"; applicationId = "com.samsung.android.ruler" }
    }

    buildTypes {
        release { isMinifyEnabled = false }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    // Coroutines: CLAUDE.md mandates them over callbacks/RxJava for camera flows.
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
}

// Rust pipeline -> app/src/main/jniLibs/arm64-v8a/librawzero_jni.so (CLAUDE.md "Build").
val cargoNdk by tasks.registering(Exec::class) {
    workingDir = rootDir
    commandLine("cargo", "ndk", "-t", "arm64-v8a", "-o", "app/src/main/jniLibs", "build", "--release", "-p", "rawzero-jni")
    inputs.files(fileTree(rootDir.resolve("core/src")), fileTree(rootDir.resolve("jni-bridge/src")), rootDir.resolve("Cargo.lock"))
    outputs.dir(layout.projectDirectory.dir("src/main/jniLibs"))
}
tasks.named("preBuild") { dependsOn(cargoNdk) }
