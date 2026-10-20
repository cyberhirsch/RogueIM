plugins {
    id("com.android.application")
}

android {
    namespace = "io.github.cyberhirsch.rogueim.phone"
    compileSdk = 36
    defaultConfig {
        applicationId = "io.github.cyberhirsch.rogueim.phone"
        minSdk = 28
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.8"
    }
    signingConfigs {
        // A release key from the environment (CI); without one, the debug key signs.
        create("release") {
            val ks = System.getenv("RIM_APK_KEYSTORE")
            if (ks != null) {
                storeFile = file(ks)
                storePassword = System.getenv("RIM_APK_PASS")
                keyAlias = "rogueim"
                keyPassword = System.getenv("RIM_APK_PASS")
            }
        }
    }
    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = if (System.getenv("RIM_APK_KEYSTORE") != null) signingConfigs.getByName("release") else signingConfigs.getByName("debug")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.17.0")
    implementation("androidx.activity:activity-ktx:1.13.0")
    implementation("com.journeyapps:zxing-android-embedded:4.3.0")
    testImplementation("junit:junit:4.13.2")
}
