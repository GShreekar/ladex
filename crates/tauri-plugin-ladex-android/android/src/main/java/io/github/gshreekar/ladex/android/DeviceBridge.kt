package io.github.gshreekar.ladex.android

import android.annotation.SuppressLint
import android.app.Activity
import android.content.Context
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.Looper
import androidx.activity.ComponentActivity
import androidx.core.net.toUri
import app.tauri.plugin.Channel
import app.tauri.plugin.JSObject
import java.io.IOException
import java.util.concurrent.ExecutionException
import java.util.concurrent.FutureTask
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

private const val MULTICAST_LOCK_TAG = "LADEX discovery"

/** A refusal in words fit for the person, sent back to Rust as {"error": ...}. */
internal class Refusal(message: String) : Exception(message)

/** What LADEX's Rust side asks of the phone, and what it is told back. Its state is only changed on the main thread. */
// The only context kept is the application's, which lives as long as the process.
@SuppressLint("StaticFieldLeak")
object DeviceBridge {
  private val mainThread = Handler(Looper.getMainLooper())
  // Also read by the questions Rust asks from its own threads.
  @Volatile private var context: Context? = null
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
      // Files shared into the app as it started arrived before anyone was listening.
      reportIncoming()
    }
  }

  internal fun incomingChanged() {
    mainThread.post { reportIncoming() }
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

  /** Opens a file shared in from another app, by its id in the last "incoming" report: {"descriptor", "size"}. */
  @JvmStatic
  fun openIncoming(json: String): String = answer {
    val opened = IncomingShare.open(appContext(), JSONObject(json).getString("id"))
    JSONObject().put("descriptor", opened.descriptor).put("size", opened.size)
  }

  @JvmStatic
  fun forgetIncoming(@Suppress("UNUSED_PARAMETER") json: String): String = answer {
    IncomingShare.forget()
    JSONObject()
  }

  /** Shows the folder picker; the folder chosen comes back later as a "saveFolder" report. */
  @JvmStatic
  fun pickSaveFolder(@Suppress("UNUSED_PARAMETER") json: String): String = answer {
    onMainThread {
      val activity = ForegroundActivity.get() as? ComponentActivity ?: throw Refusal("Open LADEX to choose a folder")
      SaveFolder.pick(activity) { name -> events?.send(JSObject().put("kind", "saveFolder").put("name", name ?: JSONObject.NULL)) }
    }
    JSONObject()
  }

  @JvmStatic
  fun saveFolder(@Suppress("UNUSED_PARAMETER") json: String): String = answer {
    JSONObject().put("name", SaveFolder.nameOf(appContext()) ?: JSONObject.NULL)
  }

  @JvmStatic
  fun forgetSaveFolder(@Suppress("UNUSED_PARAMETER") json: String): String = answer {
    SaveFolder.forget(appContext())
    JSONObject()
  }

  /** Makes a new document in the save folder: {"document": {"uri", "descriptor", "shownAs"}}, or a null document with no folder chosen. */
  @JvmStatic
  fun createInSaveFolder(json: String): String = answer {
    val created = SaveFolder.create(appContext(), JSONObject(json).getString("name"))
    val document = created?.let { JSONObject().put("uri", it.uri.toString()).put("descriptor", it.descriptor).put("shownAs", it.shownAs) }
    JSONObject().put("document", document ?: JSONObject.NULL)
  }

  @JvmStatic
  fun deleteDocument(json: String): String = answer {
    SaveFolder.delete(appContext(), JSONObject(json).getString("uri").toUri())
    JSONObject()
  }

  @JvmStatic
  fun openReceived(json: String): String = answer {
    val where = JSONObject(json).getString("where")
    onMainThread { ReceivedFiles.open(onScreenActivity(), where) }
    JSONObject()
  }

  @JvmStatic
  fun shareReceived(json: String): String = answer {
    val where = JSONObject(json).getString("where")
    onMainThread { ReceivedFiles.share(onScreenActivity(), where) }
    JSONObject()
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

  private fun reportIncoming() {
    val events = events ?: return
    val files = JSONArray()
    for (file in IncomingShare.list()) {
      files.put(
        JSONObject()
          .put("id", file.id)
          .put("name", file.name)
          .put("size", file.size ?: JSONObject.NULL)
          .put("mimeType", file.mimeType),
      )
    }
    events.send(JSObject().put("kind", "incoming").put("files", files))
  }

  private fun appContext(): Context = context ?: throw Refusal("LADEX is still starting")

  private fun onScreenActivity(): Activity = ForegroundActivity.get() ?: throw Refusal("Open LADEX first")

  // Rust asks from its own threads; starting activities and the folder picker must happen on the main thread.
  private fun <T> onMainThread(work: () -> T): T {
    if (Looper.myLooper() == Looper.getMainLooper()) return work()
    val task = FutureTask(work)
    mainThread.post(task)
    try {
      return task.get()
    } catch (error: ExecutionException) {
      throw error.cause ?: error
    }
  }

  private inline fun answer(work: () -> JSONObject): String =
    try {
      work().toString()
    } catch (error: Refusal) {
      refusal(error.message)
    } catch (error: IOException) {
      refusal("Android could not do it: ${error.message}")
    } catch (error: SecurityException) {
      refusal("Android did not allow it: ${error.message}")
    } catch (error: IllegalArgumentException) {
      refusal("Android could not do it: ${error.message}")
    } catch (error: JSONException) {
      refusal("LADEX asked Android something it did not understand: ${error.message}")
    }

  private fun refusal(message: String?): String = JSONObject().put("error", message ?: "Android could not do it").toString()

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
