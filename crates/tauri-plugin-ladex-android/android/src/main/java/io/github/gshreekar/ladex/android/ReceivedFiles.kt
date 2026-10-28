package io.github.gshreekar.ladex.android

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.ContentResolver
import android.content.Intent
import android.net.Uri
import androidx.core.content.FileProvider
import androidx.core.net.toUri
import java.io.File

/** Opening a received file in another app, or sharing it onward, through Android's intents. */
internal object ReceivedFiles {
  /** `where` is a document in the save folder (content://) or a file in LADEX's own storage (a path). */
  fun open(activity: Activity, where: String) {
    val uri = uriOf(activity, where)
    val viewing = Intent(Intent.ACTION_VIEW).setDataAndType(uri, typeOf(activity, uri)).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    try {
      activity.startActivity(viewing)
    } catch (error: ActivityNotFoundException) {
      throw Refusal("No app on this phone opens this kind of file")
    }
  }

  fun share(activity: Activity, where: String) {
    val uri = uriOf(activity, where)
    val sending = Intent(Intent.ACTION_SEND)
      .setType(typeOf(activity, uri))
      .putExtra(Intent.EXTRA_STREAM, uri)
      .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    // The chooser only passes the read permission on to the app picked if the URI is in the clip data too.
    sending.clipData = ClipData.newRawUri(null, uri)
    activity.startActivity(Intent.createChooser(sending, null))
  }

  // Files in LADEX's own storage are lent out through its FileProvider, which serves only the folders it lists.
  private fun uriOf(activity: Activity, where: String): Uri {
    if (where.startsWith("${ContentResolver.SCHEME_CONTENT}://")) return where.toUri()
    return FileProvider.getUriForFile(activity, "${activity.packageName}.fileprovider", File(where))
  }

  private fun typeOf(activity: Activity, uri: Uri): String =
    activity.contentResolver.getType(uri) ?: SaveFolder.typeOf(uri.lastPathSegment.orEmpty())
}
