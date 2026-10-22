package io.github.gshreekar.ladex.android

import android.app.Activity
import android.app.Application
import android.os.Bundle
import java.lang.ref.WeakReference

/** The activity on screen, if any: the app may only start its service, or ask for a permission, while there is one. */
internal object ForegroundActivity : Application.ActivityLifecycleCallbacks {
  private var resumed = WeakReference<Activity>(null)
  private var onResumed: (Activity) -> Unit = {}
  private var isTracking = false

  fun track(activity: Activity, onResumed: (Activity) -> Unit) {
    this.onResumed = onResumed
    // The first activity may have resumed before this was registered, and it is on screen while the app starts.
    resumed = WeakReference(activity)
    if (!isTracking) {
      activity.application.registerActivityLifecycleCallbacks(this)
      isTracking = true
    }
  }

  fun get(): Activity? = resumed.get()

  override fun onActivityResumed(activity: Activity) {
    resumed = WeakReference(activity)
    onResumed(activity)
  }

  override fun onActivityPaused(activity: Activity) {
    if (resumed.get() === activity) resumed.clear()
  }

  override fun onActivityDestroyed(activity: Activity) {
    if (resumed.get() === activity) resumed.clear()
  }

  override fun onActivityCreated(activity: Activity, savedInstanceState: Bundle?) {}

  override fun onActivityStarted(activity: Activity) {}

  override fun onActivityStopped(activity: Activity) {}

  override fun onActivitySaveInstanceState(activity: Activity, outState: Bundle) {}
}
