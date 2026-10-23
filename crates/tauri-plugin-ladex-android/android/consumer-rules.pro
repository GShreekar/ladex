# LADEX's Rust side calls this through JNI, by name.
-keep class io.github.gshreekar.ladex.android.DeviceBridge {
    public static void apply(java.lang.String);
}
