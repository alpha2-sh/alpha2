# JNA's native side and the generated bindings find these classes, fields and methods by name.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class * extends com.sun.jna.** { *; }
-keep class sh.alpha2.** { *; }
-dontwarn java.awt.**
