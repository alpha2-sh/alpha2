// `scripts/build-android.sh` builds the native libraries and bindings into `build/` first.
plugins {
    id("com.android.library")
    kotlin("android")
}

android {
    namespace = "sh.alpha2"
    compileSdk = 36
    defaultConfig {
        minSdk = 29
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }
    sourceSets["main"].kotlin.srcDir("build/generated/uniffi")
    sourceSets["main"].jniLibs.srcDir("build/jniLibs")
    sourceSets["main"].resources.srcDir("build/notices")
    sourceSets["androidTest"].assets.srcDirs("../../../tests/cases", "../../../tests/fonts")
    sourceSets["androidTest"].assets.srcDir("build/test-corpus")
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    // Native libraries stay uncompressed and 16 KB aligned in the APK.
    packaging { jniLibs { useLegacyPackaging = false } }
}

dependencies {
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.10.2")
}
