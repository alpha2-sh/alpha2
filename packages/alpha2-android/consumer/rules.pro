# The test's entry into the app, and the Kotlin library the test APK takes from the app; the
# library under test gets only the rules its AAR ships.
-keep class sh.alpha2.consumer.Probe { *; }
-keep class kotlin.** { *; }
