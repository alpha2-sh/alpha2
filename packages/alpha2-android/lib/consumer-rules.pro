# JNA and the generated bindings reach native code by class, field and method names: JNA's native
# side looks up its own fields, structures are read by reflection, callback interfaces and the
# library interface are bound by method name.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class * extends com.sun.jna.** { *; }
-keep class sh.alpha2.** { *; }
-dontwarn java.awt.**
