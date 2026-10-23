package io.github.gshreekar.ladex.android

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.Handler
import android.os.Looper
import java.net.Inet4Address
import java.net.NetworkInterface
import java.net.SocketException
import java.util.Collections

private const val RECHECK_MILLIS = 5_000L

// Hotspot and tethering interfaces; some phones run their hotspot on a second "wlan" interface.
private val SHARED_INTERFACE_PREFIXES = listOf("ap", "swlan", "softap", "wlan", "rndis", "ncm")

/** Watches for the networks other LADEX devices can be on: Wi-Fi or Ethernet, or this phone's own hotspot or tethering. */
internal class NetworkWatcher(context: Context, private val onChange: () -> Unit) {
  private val connectivity = context.getSystemService(ConnectivityManager::class.java)
  private val mainThread = Handler(Looper.getMainLooper())

  // Changed on Android's network thread, read on the main thread.
  private val localNetworks: MutableSet<Network> = Collections.synchronizedSet(HashSet())

  private val callback = object : ConnectivityManager.NetworkCallback() {
    override fun onAvailable(network: Network) {
      localNetworks.add(network)
      mainThread.post { onChange() }
    }

    override fun onLost(network: Network) {
      localNetworks.remove(network)
      mainThread.post { onChange() }
    }
  }

  // Turning a hotspot on or off is not a network change apps hear about, so it is looked for every few seconds.
  private val recheck = object : Runnable {
    override fun run() {
      onChange()
      mainThread.postDelayed(this, RECHECK_MILLIS)
    }
  }

  fun start() {
    val request = NetworkRequest.Builder()
      .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
      .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
      // A router with no internet still connects the devices on it.
      .removeCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
      .build()
    connectivity.registerNetworkCallback(request, callback)
    mainThread.postDelayed(recheck, RECHECK_MILLIS)
  }

  fun isOnWifi(): Boolean = localNetworks.isNotEmpty()

  /** Off Wi-Fi, whether this phone shares its own hotspot or tethering, which Android does not report to apps as a network. */
  fun sharesOwnNetwork(): Boolean {
    if (isOnWifi()) return false
    return try {
      NetworkInterface.getNetworkInterfaces()?.toList().orEmpty().any { it.isSharedNetwork() }
    } catch (error: SocketException) {
      false
    }
  }
}

private fun NetworkInterface.isSharedNetwork(): Boolean =
  isUp && SHARED_INTERFACE_PREFIXES.any { name.startsWith(it) } && inetAddresses.toList().any { it is Inet4Address }
