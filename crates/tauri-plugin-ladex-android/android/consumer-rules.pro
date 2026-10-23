# LADEX's Rust side calls these through JNI, by name: `apply`, and the questions that answer in JSON.
-keep class io.github.gshreekar.ladex.android.DeviceBridge {
    public static void apply(java.lang.String);
    public static java.lang.String *(java.lang.String);
}
