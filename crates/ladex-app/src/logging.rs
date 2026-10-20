//! Where the app's logs go: the terminal on desktop, logcat on Android.

#[cfg(not(target_os = "android"))]
pub fn init() {
    tracing_subscriber::fmt::init();
}

#[cfg(target_os = "android")]
pub fn init() {
    use tracing_logcat::{LogcatMakeWriter, LogcatTag};
    use tracing_subscriber::fmt::format::Format;

    // Without logd there is nowhere left to report the failure, so the app runs without logs.
    let Ok(writer) = LogcatMakeWriter::new(LogcatTag::Fixed("LADEX".to_string())) else { return };
    // logcat stamps each line with its own time and level.
    tracing_subscriber::fmt().event_format(Format::default().with_level(false).without_time()).with_writer(writer).with_ansi(false).init();
}
