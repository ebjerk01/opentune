# Keep JNA and the UniFFI-generated bindings intact (reflection / native).
-dontwarn java.awt.**
-keep class com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { public *; }
-keep class org.opentune.ffi.** { *; }
