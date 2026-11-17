// An app whose release build R8 shrinks with only the rules the library's AAR ships; its
// instrumentation test runs against that build.
plugins {
    id("com.android.application")
    kotlin("android")
}

android {
    namespace = "sh.alpha2.consumer"
    compileSdk = 36
    defaultConfig {
        applicationId = "sh.alpha2.consumer"
        minSdk = 29
        targetSdk = 36
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }
    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "rules.pro")
            testProguardFiles("test-rules.pro")
            signingConfig = signingConfigs.getByName("debug")
        }
    }
    testBuildType = "release"
    sourceSets["androidTest"].assets.srcDirs("../../../tests/cases", "../lib/build/test-corpus")
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    packaging { jniLibs { useLegacyPackaging = false } }
}

dependencies {
    implementation(project(":lib"))
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
}
