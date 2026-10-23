package io.github.gshreekar.ladex.android

import android.annotation.SuppressLint
import android.app.Activity
import android.content.Context
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.Looper
import app.tauri.plugin.Channel
import app.tauri.plugin.JSObject
import org.json.JSONObject

private const val MULTICAST_LOCK_TAG = "LADEX discovery"

/** What LADEX's Rust side asks of the phone, and what it is told back. Its state is only touched on the main thread. */
// The only context kept is the application's, which lives as long as the process.
@SuppressLint("StaticFieldLeak")
object DeviceBridge {
  private val mainThread = Handler(Looper.getMainLooper())
  private var context: Context? = null
  private var events: Channel? = null
  private var network: NetworkWatcher? = null
  private var multicastLock: WifiManager.MulticastLock? = null
  private var lastReport: String? = null

  internal fun attach(activity: Activity) {
    mainThread.post {
      context = activity.applicationContext
      ForegroundActivity.track(activity) { resumed -> onScreen(resumed) }
    }
  }

  internal fun watch(events: Channel) {
    mainThread.post {
      this.events = events
      val context = context ?: return@post
      if (network == null) {
        network = NetworkWatcher(context) { report() }.also { it.start() }
      }
      report()
    }
  }

  /** Called from Rust through JNI with what the phone should do now; a malformed request throws back to Rust. */
  @JvmStatic
  fun apply(json: String) {
    val wanted = Wanted.parse(json)
    mainThread.post { applyNow(wanted) }
  }

  internal fun stopPressed() {
    events?.send(JSObject().put("kind", "stop"))
  }

  // Android may have closed the app's window while sharing went on; Rust opens a new one if so.
  private fun onScreen(activity: Activity) {
    SharingService.showAgain(activity.applicationContext)
    events?.send(JSObject().put("kind", "shown"))
  }

  private fun applyNow(wanted: Wanted) {
    val context = context ?: return
    holdMulticastLock(context, wanted.holdsMulticastLock)
    SharingService.show(context, wanted.sharing)
    report()
  }

  private fun holdMulticastLock(context: Context, shouldHold: Boolean) {
    val lock = multicastLock ?: newMulticastLock(context)?.also { multicastLock = it } ?: return
    if (shouldHold && !lock.isHeld) lock.acquire()
    if (!shouldHold && lock.isHeld) lock.release()
  }

  // A phone without Wi-Fi has no multicast to let through.
  private fun newMulticastLock(context: Context): WifiManager.MulticastLock? {
    val wifi = context.getSystemService(Context.WIFI_SERVICE) as? WifiManager ?: return null
    return wifi.createMulticastLock(MULTICAST_LOCK_TAG).apply { setReferenceCounted(false) }
  }

  // Sent only when something changed, so Rust can act on every report it gets.
  private fun report() {
    val events = events ?: return
    val network = network ?: return
    val report = JSObject()
      .put("kind", "network")
      .put("isOnWifi", network.isOnWifi())
      .put("sharesOwnNetwork", network.sharesOwnNetwork())
      .put("holdsMulticastLock", multicastLock?.isHeld == true)
    val text = report.toString()
    if (text == lastReport) return
    lastReport = text
    events.send(report)
  }
}

internal data class SharingNotice(val text: String, val keepsAwake: Boolean)

/** What Rust wants of the phone, as `sharing.rs` in the app writes it. */
internal class Wanted(val holdsMulticastLock: Boolean, val sharing: SharingNotice?) {
  companion object {
    fun parse(json: String): Wanted {
      val wanted = JSONObject(json)
      val sharing = wanted.optJSONObject("sharing")?.let { SharingNotice(it.getString("text"), it.getBoolean("keepsAwake")) }
      return Wanted(wanted.getBoolean("holdsMulticastLock"), sharing)
    }
  }
}
