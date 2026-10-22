//! What a phone does about sharing: what its notification says, when seeding pauses, and what the Network page reports.
//! The JSON matches `DeviceBridge.kt` in the Android plugin.

use std::time::{Duration, Instant};

use ladex_core::doctor::{check, Check, Verdict};
use serde::{Deserialize, Serialize};

/// How long the phone may be off its network before seeding pauses; a dropped Wi-Fi connection often comes straight back.
pub const PAUSE_GRACE: Duration = Duration::from_secs(10);
/// How long uploading counts as a transfer after the last upload was seen: the upload slots empty between chunk requests.
pub const UPLOAD_LINGER: Duration = Duration::from_secs(15);

/// What Android reports about the network the phone is on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkReport {
    /// On Wi-Fi or Ethernet.
    pub is_on_wifi: bool,
    /// Off Wi-Fi, sharing its own hotspot or tethering.
    pub shares_own_network: bool,
    pub holds_multicast_lock: bool,
}

impl NetworkReport {
    /// Whether other devices can reach the phone: over Wi-Fi or Ethernet, or on its own hotspot.
    pub fn is_on_local_network(&self) -> bool {
        self.is_on_wifi || self.shares_own_network
    }
}

/// Something that happened on the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DeviceEvent {
    Network(NetworkReport),
    /// The app came on screen, perhaps in an activity Android made after closing the window.
    Shown,
    /// The Stop button in the sharing notification.
    Stop,
}

/// What the phone should do now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Wanted {
    pub holds_multicast_lock: bool,
    /// The ongoing notification that keeps LADEX sharing outside the app; None stops it.
    pub sharing: Option<SharingNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharingNotice {
    pub text: String,
    /// Keeps the processor and Wi-Fi awake, so a transfer goes on with the screen off.
    pub keeps_awake: bool,
}

/// What the node is doing, which decides what the phone should do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Situation {
    pub is_node_running: bool,
    pub keeps_sharing_in_background: bool,
    pub is_paused: bool,
    pub other_devices: usize,
    pub downloads: usize,
    pub is_uploading: bool,
}

/// The multicast lock while the node runs, and the sharing notification while transfers run or the person asked for it.
pub fn wanted(situation: &Situation) -> Wanted {
    Wanted { holds_multicast_lock: situation.is_node_running, sharing: sharing_notice(situation) }
}

// A transfer keeps its notification through a pause, so it carries on once the phone is back on Wi-Fi.
fn sharing_notice(situation: &Situation) -> Option<SharingNotice> {
    let has_transfers = situation.downloads > 0 || situation.is_uploading;
    if !situation.is_node_running || !(has_transfers || situation.keeps_sharing_in_background) {
        return None;
    }
    Some(SharingNotice { text: notice_text(situation), keeps_awake: has_transfers && !situation.is_paused })
}

fn notice_text(situation: &Situation) -> String {
    if situation.is_paused {
        return "Paused until this phone is back on Wi-Fi".to_string();
    }
    let sharing = match situation.other_devices {
        0 => "Waiting for other devices".to_string(),
        1 => "Sharing with 1 device".to_string(),
        count => format!("Sharing with {count} devices"),
    };
    match situation.downloads {
        0 => sharing,
        1 => format!("Downloading 1 file · {sharing}"),
        count => format!("Downloading {count} files · {sharing}"),
    }
}

/// Whether seeding is paused: once the phone has been off its network for `PAUSE_GRACE`, until it is back on one.
#[derive(Debug, Default)]
pub struct PauseClock {
    off_network_since: Option<Instant>,
    is_paused: bool,
}

impl PauseClock {
    pub fn is_paused(&self) -> bool {
        self.is_paused
    }

    /// Takes a network report; true when the phone is back on a network after a pause, so seeding should resume.
    pub fn report(&mut self, report: &NetworkReport, now: Instant) -> bool {
        if report.is_on_local_network() {
            self.off_network_since = None;
            return std::mem::take(&mut self.is_paused);
        }
        self.off_network_since.get_or_insert(now);
        false
    }

    /// True once the phone has been off its network for the grace period, so seeding should pause.
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(since) = self.off_network_since else { return false };
        if self.is_paused || now.duration_since(since) < PAUSE_GRACE {
            return false;
        }
        self.is_paused = true;
        true
    }
}

/// Whether uploads ran lately, looked at every few seconds.
#[derive(Debug, Default)]
pub struct UploadWatch {
    last_seen: Option<Instant>,
}

impl UploadWatch {
    pub fn is_uploading(&mut self, uploads_running: usize, now: Instant) -> bool {
        if uploads_running > 0 {
            self.last_seen = Some(now);
        }
        self.last_seen.is_some_and(|seen| now.duration_since(seen) < UPLOAD_LINGER)
    }
}

/// The phone's own checks for the Network page: whether it is on Wi-Fi, and whether Android lets discovery through.
pub fn checks(report: Option<&NetworkReport>, is_paused: bool) -> Vec<Check> {
    let Some(report) = report else {
        return vec![check("Wi-Fi", Verdict::Warning, "Android has not reported the network yet; check again in a moment")];
    };
    vec![wifi_check(report, is_paused), multicast_check(report)]
}

fn wifi_check(report: &NetworkReport, is_paused: bool) -> Check {
    if report.is_on_wifi {
        return check("Wi-Fi", Verdict::Ok, "connected");
    }
    if report.shares_own_network {
        return check("Wi-Fi", Verdict::Ok, "this phone shares its own hotspot or tethering, so other devices can join it");
    }
    let detail = if is_paused {
        "not connected, so sharing is paused until this phone is back on Wi-Fi"
    } else {
        "not connected: other devices are only reached over Wi-Fi, and sharing pauses if it stays off"
    };
    check("Wi-Fi", Verdict::Problem, detail)
}

fn multicast_check(report: &NetworkReport) -> Check {
    if report.holds_multicast_lock {
        return check("Multicast lock", Verdict::Ok, "held, so Android lets discovery through");
    }
    check(
        "Multicast lock",
        Verdict::Problem,
        "not held, so Android drops discovery and other devices cannot find this phone; add them by address on this page",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sharing(situation: Situation) -> Option<SharingNotice> {
        wanted(&situation).sharing
    }

    fn running() -> Situation {
        Situation { is_node_running: true, ..Situation::default() }
    }

    fn on_wifi() -> NetworkReport {
        NetworkReport { is_on_wifi: true, shares_own_network: false, holds_multicast_lock: true }
    }

    fn off_wifi() -> NetworkReport {
        NetworkReport { is_on_wifi: false, shares_own_network: false, holds_multicast_lock: true }
    }

    fn verdict_of(checks: &[Check], name: &str) -> Verdict {
        checks.iter().find(|check| check.name == name).expect("the check exists").verdict
    }

    #[test]
    fn the_multicast_lock_is_held_while_the_node_runs() {
        assert!(wanted(&running()).holds_multicast_lock);
        assert!(!wanted(&Situation::default()).holds_multicast_lock);
    }

    #[test]
    fn an_idle_phone_shares_only_while_the_app_is_open() {
        assert_eq!(sharing(running()), None);
    }

    #[test]
    fn sharing_in_the_background_shows_the_devices_it_shares_with() {
        let situation = Situation { keeps_sharing_in_background: true, other_devices: 3, ..running() };
        assert_eq!(sharing(situation), Some(SharingNotice { text: "Sharing with 3 devices".into(), keeps_awake: false }));
    }

    #[test]
    fn one_device_is_named_in_the_singular() {
        let situation = Situation { keeps_sharing_in_background: true, other_devices: 1, ..running() };
        assert_eq!(sharing(situation).unwrap().text, "Sharing with 1 device");
    }

    #[test]
    fn a_phone_alone_says_it_is_waiting() {
        let situation = Situation { keeps_sharing_in_background: true, ..running() };
        assert_eq!(sharing(situation).unwrap().text, "Waiting for other devices");
    }

    #[test]
    fn a_download_keeps_the_phone_awake_and_says_so() {
        let situation = Situation { downloads: 2, other_devices: 1, ..running() };
        assert_eq!(
            sharing(situation),
            Some(SharingNotice { text: "Downloading 2 files · Sharing with 1 device".into(), keeps_awake: true })
        );
    }

    #[test]
    fn an_upload_keeps_the_phone_awake() {
        let situation = Situation { is_uploading: true, other_devices: 1, ..running() };
        assert!(sharing(situation).unwrap().keeps_awake);
    }

    #[test]
    fn a_paused_download_keeps_its_notification_but_lets_the_phone_sleep() {
        let situation = Situation { downloads: 1, is_paused: true, ..running() };
        assert_eq!(sharing(situation), Some(SharingNotice { text: "Paused until this phone is back on Wi-Fi".into(), keeps_awake: false }));
    }

    #[test]
    fn nothing_is_shared_once_the_node_has_stopped() {
        let situation = Situation { is_node_running: false, keeps_sharing_in_background: true, downloads: 1, ..Situation::default() };
        assert_eq!(sharing(situation), None);
    }

    #[test]
    fn what_the_phone_is_asked_is_written_as_the_kotlin_side_reads_it() {
        let situation = Situation { downloads: 1, other_devices: 1, ..running() };
        let json = serde_json::to_value(wanted(&situation)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "holdsMulticastLock": true,
                "sharing": { "text": "Downloading 1 file · Sharing with 1 device", "keepsAwake": true }
            })
        );
    }

    #[test]
    fn no_notification_is_written_as_null() {
        let json = serde_json::to_value(wanted(&running())).unwrap();
        assert_eq!(json["sharing"], serde_json::Value::Null);
    }

    #[test]
    fn a_network_report_from_the_kotlin_side_is_read() {
        let json = r#"{"kind":"network","isOnWifi":false,"sharesOwnNetwork":true,"holdsMulticastLock":true}"#;
        let event: DeviceEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event, DeviceEvent::Network(NetworkReport { is_on_wifi: false, shares_own_network: true, holds_multicast_lock: true }));
    }

    #[test]
    fn the_stop_button_from_the_kotlin_side_is_read() {
        assert_eq!(serde_json::from_str::<DeviceEvent>(r#"{"kind":"stop"}"#).unwrap(), DeviceEvent::Stop);
    }

    #[test]
    fn the_app_coming_on_screen_is_read() {
        assert_eq!(serde_json::from_str::<DeviceEvent>(r#"{"kind":"shown"}"#).unwrap(), DeviceEvent::Shown);
    }

    #[test]
    fn a_phone_on_its_own_hotspot_is_on_a_local_network() {
        assert!(NetworkReport { is_on_wifi: false, shares_own_network: true, holds_multicast_lock: true }.is_on_local_network());
    }

    #[test]
    fn seeding_does_not_pause_before_the_grace_period_ends() {
        let mut clock = PauseClock::default();
        let left = Instant::now();
        clock.report(&off_wifi(), left);
        assert!(!clock.tick(left + PAUSE_GRACE - Duration::from_secs(1)));
    }

    #[test]
    fn seeding_pauses_once_after_the_grace_period() {
        let mut clock = PauseClock::default();
        let left = Instant::now();
        clock.report(&off_wifi(), left);
        assert!(clock.tick(left + PAUSE_GRACE));
        assert!(!clock.tick(left + PAUSE_GRACE * 2));
        assert!(clock.is_paused());
    }

    #[test]
    fn coming_back_within_the_grace_period_never_pauses() {
        let mut clock = PauseClock::default();
        let left = Instant::now();
        clock.report(&off_wifi(), left);
        assert!(!clock.report(&on_wifi(), left + Duration::from_secs(3)));
        assert!(!clock.tick(left + PAUSE_GRACE * 2));
    }

    #[test]
    fn coming_back_after_a_pause_resumes_once() {
        let mut clock = PauseClock::default();
        let left = Instant::now();
        clock.report(&off_wifi(), left);
        clock.tick(left + PAUSE_GRACE);
        assert!(clock.report(&on_wifi(), left + PAUSE_GRACE * 2));
        assert!(!clock.report(&on_wifi(), left + PAUSE_GRACE * 3));
        assert!(!clock.is_paused());
    }

    #[test]
    fn repeated_reports_off_wifi_keep_the_time_it_was_left() {
        let mut clock = PauseClock::default();
        let left = Instant::now();
        clock.report(&off_wifi(), left);
        clock.report(&off_wifi(), left + Duration::from_secs(8));
        assert!(clock.tick(left + PAUSE_GRACE));
    }

    #[test]
    fn an_upload_counts_for_a_while_after_it_was_last_seen() {
        let mut watch = UploadWatch::default();
        let seen = Instant::now();
        watch.is_uploading(1, seen);
        assert!(watch.is_uploading(0, seen + UPLOAD_LINGER - Duration::from_secs(1)));
        assert!(!watch.is_uploading(0, seen + UPLOAD_LINGER));
    }

    #[test]
    fn a_phone_on_wifi_holding_the_lock_passes_both_checks() {
        let checks = checks(Some(&on_wifi()), false);
        assert!(checks.iter().all(|check| check.verdict == Verdict::Ok), "{checks:?}");
    }

    #[test]
    fn a_phone_off_wifi_is_a_problem_that_says_sharing_is_paused() {
        let checks = checks(Some(&off_wifi()), true);
        let wifi = checks.iter().find(|check| check.name == "Wi-Fi").unwrap();
        assert_eq!(wifi.verdict, Verdict::Problem);
        assert!(wifi.detail.contains("paused"), "{}", wifi.detail);
    }

    #[test]
    fn a_phone_sharing_its_hotspot_passes_the_wifi_check() {
        let report = NetworkReport { is_on_wifi: false, shares_own_network: true, holds_multicast_lock: true };
        assert_eq!(verdict_of(&checks(Some(&report), false), "Wi-Fi"), Verdict::Ok);
    }

    #[test]
    fn a_missing_multicast_lock_is_a_problem() {
        let report = NetworkReport { holds_multicast_lock: false, ..on_wifi() };
        assert_eq!(verdict_of(&checks(Some(&report), false), "Multicast lock"), Verdict::Problem);
    }

    #[test]
    fn before_android_reports_the_wifi_check_is_a_warning() {
        assert_eq!(verdict_of(&checks(None, false), "Wi-Fi"), Verdict::Warning);
    }
}
