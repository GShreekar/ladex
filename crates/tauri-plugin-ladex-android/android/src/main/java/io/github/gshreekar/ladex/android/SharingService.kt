package io.github.gshreekar.ladex.android

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat

private const val TAG = "LADEX"
private const val NOTIFICATION_ID = 7878
private const val CHANNEL_ID = "sharing"
private const val CHANNEL_NAME = "Sharing in the background"
private const val TITLE = "LADEX"
private const val STOP_LABEL = "Stop"
private const val NOTIFICATION_PERMISSION_REQUEST = 7878

/** Keeps LADEX sharing outside the app, under an ongoing notification with a Stop button. Used on the main thread only. */
class SharingService : Service() {
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onCreate() {
    super.onCreate()
    running = this
  }

  // ServiceCompat leaves the service type out below Android 10, which has none.
  @SuppressLint("InlinedApi")
  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    isStartPending = false
    // A service started this way must reach the foreground before it may stop, even if it is no longer wanted.
    val notice = wanted ?: startedFor ?: return START_NOT_STICKY
    try {
      ServiceCompat.startForeground(this, NOTIFICATION_ID, notification(this, notice), ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
    } catch (error: IllegalStateException) {
      // Android 12+ refuses an app in the background, and Android 15+ one that used up its six hours today.
      Log.w(TAG, "Could not keep sharing in the background: $error")
      stopSelf()
      return START_NOT_STICKY
    }
    if (wanted == null) {
      stopNow()
    } else {
      TransferLocks.hold(this, notice.keepsAwake)
    }
    return START_NOT_STICKY
  }

  // Android 15+ lets a dataSync service run six hours a day; it starts again when the app is next opened.
  override fun onTimeout(startId: Int, fgsType: Int) {
    stopNow()
  }

  override fun onDestroy() {
    running = null
    TransferLocks.hold(this, false)
    super.onDestroy()
  }

  private fun update(notice: SharingNotice) {
    // Without the notification permission Android drops this quietly; the service keeps running all the same.
    getSystemService(NotificationManager::class.java)?.notify(NOTIFICATION_ID, notification(this, notice))
    TransferLocks.hold(this, notice.keepsAwake)
  }

  private fun stopNow() {
    ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
    stopSelf()
  }

  companion object {
    private var running: SharingService? = null
    private var isStartPending = false
    private var wanted: SharingNotice? = null
    private var startedFor: SharingNotice? = null
    private var hasAskedForNotifications = false

    /** Shows [notice] in the ongoing notification, starting the service if needed; null stops it. */
    internal fun show(context: Context, notice: SharingNotice?) {
      wanted = notice
      val service = running
      when {
        service != null && notice != null -> service.update(notice)
        service != null -> service.stopNow()
        notice != null -> start(context, notice)
      }
    }

    /**
     * Starts the service if it was wanted while the app was in the background, where Android does not allow it, or posts
     * its notification again: one posted before the notification permission was granted is dropped, not kept.
     */
    internal fun showAgain(context: Context) {
      val notice = wanted ?: return
      val service = running
      if (service == null) start(context, notice) else service.update(notice)
    }

    private fun start(context: Context, notice: SharingNotice) {
      if (isStartPending) return
      val activity = ForegroundActivity.get() ?: return
      askForNotificationsOnce(activity)
      try {
        ContextCompat.startForegroundService(context, Intent(context, SharingService::class.java))
        startedFor = notice
        isStartPending = true
      } catch (error: IllegalStateException) {
        Log.w(TAG, "Could not start sharing in the background: $error")
      }
    }

    // Asked when first needed; a refusal only hides the notification, the sharing goes on.
    private fun askForNotificationsOnce(activity: Activity) {
      if (hasAskedForNotifications || Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
      hasAskedForNotifications = true
      val permission = Manifest.permission.POST_NOTIFICATIONS
      if (ContextCompat.checkSelfPermission(activity, permission) == PackageManager.PERMISSION_GRANTED) return
      ActivityCompat.requestPermissions(activity, arrayOf(permission), NOTIFICATION_PERMISSION_REQUEST)
    }
  }
}

/** The notification's Stop button: LADEX says goodbye to the other devices and quits. */
class StopReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent) {
    DeviceBridge.stopPressed()
  }
}

private fun notification(context: Context, notice: SharingNotice): Notification {
  val channel = NotificationChannelCompat.Builder(CHANNEL_ID, NotificationManagerCompat.IMPORTANCE_LOW).setName(CHANNEL_NAME).build()
  NotificationManagerCompat.from(context).createNotificationChannel(channel)
  return NotificationCompat.Builder(context, CHANNEL_ID)
    .setSmallIcon(R.drawable.ic_ladex_sharing)
    .setContentTitle(TITLE)
    .setContentText(notice.text)
    .setOngoing(true)
    .setOnlyAlertOnce(true)
    .setSilent(true)
    .setCategory(NotificationCompat.CATEGORY_SERVICE)
    .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
    .setContentIntent(openAppIntent(context))
    .addAction(0, STOP_LABEL, stopIntent(context))
    .build()
}

private fun openAppIntent(context: Context): PendingIntent? {
  val launch = context.packageManager.getLaunchIntentForPackage(context.packageName) ?: return null
  return PendingIntent.getActivity(context, 0, launch, PendingIntent.FLAG_IMMUTABLE)
}

private fun stopIntent(context: Context): PendingIntent =
  PendingIntent.getBroadcast(context, 0, Intent(context, StopReceiver::class.java), PendingIntent.FLAG_IMMUTABLE)

/** Keeps the processor and Wi-Fi awake while a transfer runs with the screen off. */
internal object TransferLocks {
  private var wakeLock: PowerManager.WakeLock? = null
  private var wifiLock: WifiManager.WifiLock? = null

  // Released as soon as no transfer runs or the service stops, so the locks need no timeout.
  @SuppressLint("WakelockTimeout", "Wakelock")
  fun hold(context: Context, shouldHold: Boolean) {
    val wake = wakeLock ?: newWakeLock(context)?.also { wakeLock = it }
    if (wake != null && shouldHold && !wake.isHeld) wake.acquire()
    if (wake != null && !shouldHold && wake.isHeld) wake.release()
    val wifi = wifiLock ?: newWifiLock(context)?.also { wifiLock = it }
    if (wifi != null && shouldHold && !wifi.isHeld) wifi.acquire()
    if (wifi != null && !shouldHold && wifi.isHeld) wifi.release()
  }

  private fun newWakeLock(context: Context): PowerManager.WakeLock? =
    context.getSystemService(PowerManager::class.java)
      ?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "LADEX:transfer")
      ?.apply { setReferenceCounted(false) }

  // High-performance mode keeps Wi-Fi at full speed with the screen off.
  @Suppress("DEPRECATION")
  private fun newWifiLock(context: Context): WifiManager.WifiLock? =
    (context.applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager)
      ?.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "LADEX transfer")
      ?.apply { setReferenceCounted(false) }
}
