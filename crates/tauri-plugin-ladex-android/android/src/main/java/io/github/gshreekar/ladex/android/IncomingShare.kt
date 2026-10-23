package io.github.gshreekar.ladex.android

import android.content.ContentResolver
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import android.util.Log
import android.widget.Toast
import androidx.core.content.IntentCompat
import java.io.File
import java.io.FileNotFoundException
import kotlin.concurrent.thread

private const val MAX_FILES = 500
private const val UNKNOWN_TYPE = "application/octet-stream"
private const val LOG_TAG = "LADEX"

/** One file another app shared into LADEX: a content:// URI, never assumed to be a file on disk. */
internal class IncomingFile(val id: String, val uri: Uri, val name: String, val size: Long?, val mimeType: String)

/** A shared file opened for reading; Rust takes over the descriptor and closes it. */
internal class OpenedFile(val descriptor: Int, val size: Long)

/** Files shared into LADEX from another app's share sheet, kept until the person shares them or sets them aside. */
object IncomingShare {
  private val waiting = linkedMapOf<String, IncomingFile>()
  private var sharesReceived = 0

  /** Called by the app's activity with each intent that starts it or reaches it while it runs. */
  @JvmStatic
  fun receive(context: Context, intent: Intent?) {
    if (intent == null || !isShare(intent)) return
    // Opening the app again from Recents hands it the intent that first started it, which was dealt with then.
    if (intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY != 0) return
    val uris = sharedUris(context, intent)
    if (uris.isEmpty()) {
      Toast.makeText(context, "LADEX shares files, and nothing shared was a file", Toast.LENGTH_LONG).show()
      return
    }
    val resolver = context.contentResolver
    // Asking another app's provider about its files can be slow, so it is kept off the main thread.
    thread(name = "LADEX share sheet") {
      val share = synchronized(this) { ++sharesReceived }
      val files = uris.mapIndexed { index, uri -> describe(resolver, uri, "share$share-$index") }
      synchronized(this) {
        waiting.clear()
        files.forEach { waiting[it.id] = it }
      }
      DeviceBridge.incomingChanged()
    }
  }

  internal fun list(): List<IncomingFile> = synchronized(this) { waiting.values.toList() }

  internal fun forget() {
    synchronized(this) { waiting.clear() }
    DeviceBridge.incomingChanged()
  }

  /** Opens a waiting file; one with no size up front (a pipe from another app) is copied into the cache first, because the store needs its size. */
  internal fun open(context: Context, id: String): OpenedFile {
    val file = synchronized(this) { waiting[id] } ?: throw Refusal("That file was set aside; share it into LADEX again")
    val descriptor = context.contentResolver.openFileDescriptor(file.uri, "r") ?: throw FileNotFoundException("${file.name} could not be opened")
    if (descriptor.statSize >= 0) return OpenedFile(descriptor.detachFd(), descriptor.statSize)
    descriptor.close()
    return copiedIntoCache(context, file)
  }

  private fun copiedIntoCache(context: Context, file: IncomingFile): OpenedFile {
    val copy = File.createTempFile("incoming", null, context.cacheDir)
    try {
      val input = context.contentResolver.openInputStream(file.uri) ?: throw FileNotFoundException("${file.name} could not be read")
      input.use { source -> copy.outputStream().use { source.copyTo(it) } }
      val descriptor = ParcelFileDescriptor.open(copy, ParcelFileDescriptor.MODE_READ_ONLY)
      return OpenedFile(descriptor.detachFd(), copy.length())
    } finally {
      // The open descriptor keeps the bytes readable, so the name goes at once and nothing is left behind.
      copy.delete()
    }
  }

  private fun isShare(intent: Intent): Boolean = intent.action == Intent.ACTION_SEND || intent.action == Intent.ACTION_SEND_MULTIPLE

  private fun sharedUris(context: Context, intent: Intent): List<Uri> {
    val streams = if (intent.action == Intent.ACTION_SEND) {
      listOfNotNull(IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java))
    } else {
      IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
    }
    // Some apps put the files only in the clip data.
    val clip = intent.clipData
    val clipped = if (clip == null) emptyList() else (0 until clip.itemCount).mapNotNull { clip.getItemAt(it).uri }
    return (streams + clipped).distinct().filter { isFromAnotherApp(context, it) }.take(MAX_FILES)
  }

  // A file:// URI, or one served by LADEX itself, would have LADEX read its own private files on another app's behalf.
  private fun isFromAnotherApp(context: Context, uri: Uri): Boolean {
    if (uri.scheme != ContentResolver.SCHEME_CONTENT) return false
    val authority = uri.authority ?: return false
    return !authority.startsWith(context.packageName)
  }

  private fun describe(resolver: ContentResolver, uri: Uri, id: String): IncomingFile {
    var name: String? = null
    var size: Long? = null
    try {
      resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use { row ->
        if (row.moveToFirst()) {
          val nameColumn = row.getColumnIndex(OpenableColumns.DISPLAY_NAME)
          val sizeColumn = row.getColumnIndex(OpenableColumns.SIZE)
          if (nameColumn >= 0 && !row.isNull(nameColumn)) name = row.getString(nameColumn)
          if (sizeColumn >= 0 && !row.isNull(sizeColumn)) size = row.getLong(sizeColumn)
        }
      }
    } catch (error: SecurityException) {
      // Still listed: opening it later says what went wrong, in the dialog where the person can see it.
      Log.w(LOG_TAG, "Could not ask about a shared file: $error")
    }
    val type = resolver.getType(uri) ?: UNKNOWN_TYPE
    return IncomingFile(id, uri, name ?: uri.lastPathSegment ?: "Shared file", size, type)
  }
}
