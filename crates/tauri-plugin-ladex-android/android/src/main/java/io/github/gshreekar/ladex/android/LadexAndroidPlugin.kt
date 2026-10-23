package io.github.gshreekar.ladex.android

import android.app.Activity
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

@InvokeArg
class WatchArgs {
  lateinit var events: Channel
}

/** Where Tauri meets LADEX's Android code; the work is done in [DeviceBridge]. */
@TauriPlugin
class LadexAndroidPlugin(activity: Activity) : Plugin(activity) {
  init {
    DeviceBridge.attach(activity)
  }

  @Command
  fun watch(invoke: Invoke) {
    val args = invoke.parseArgs(WatchArgs::class.java)
    DeviceBridge.watch(args.events)
    invoke.resolve()
  }
}
