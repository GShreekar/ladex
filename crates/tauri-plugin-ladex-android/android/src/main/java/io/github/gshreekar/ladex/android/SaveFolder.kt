package io.github.gshreekar.ladex.android

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.util.Log
import android.webkit.MimeTypeMap
import androidx.activity.ComponentActivity
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.edit
import androidx.core.net.toUri
import java.io.FileNotFoundException
import java.io.IOException

private const val PREFERENCES = "ladex_save_folder"
private const val KEY_TREE = "tree"
private const val PICKER_KEY = "ladex_pick_save_folder"
private const val EXTERNAL_STORAGE = "com.android.externalstorage.documents"
private const val UNKNOWN_TYPE = "application/octet-stream"
private const val LOG_TAG = "LADEX"
private const val READ_AND_WRITE = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION

/** A new document in the save folder, open for writing; Rust takes over the descriptor and closes it. */
internal class CreatedDocument(val uri: Uri, val descriptor: Int, val shownAs: String)

/** The folder the person chose for received files, through the Storage Access Framework; LADEX keeps its permission. */
internal object SaveFolder {
  /** Shows Android's folder picker; `onPicked` gets the folder's name, or null if none is chosen. Main thread only. */
  fun pick(activity: ComponentActivity, onPicked: (String?) -> Unit) {
    lateinit var launcher: ActivityResultLauncher<Uri?>
    launcher = activity.activityResultRegistry.register(PICKER_KEY, ActivityResultContracts.OpenDocumentTree()) { tree ->
      launcher.unregister()
      val context = activity.applicationContext
      if (tree != null) remember(context, tree)
      onPicked(nameOf(context))
    }
    launcher.launch(chosenTree(activity))
  }

  /** The chosen folder's name, or null if there is none or Android has taken back the permission. */
  fun nameOf(context: Context): String? = chosenTree(context)?.let { describe(context, it) }

  fun forget(context: Context) {
    val tree = chosenTree(context) ?: return
    release(context, tree)
    preferences(context).edit { remove(KEY_TREE) }
  }

  /** Makes a new document called `name` in the save folder, which may rename it to keep names unique; null with no folder chosen. */
  fun create(context: Context, name: String): CreatedDocument? {
    val tree = chosenTree(context) ?: return null
    val resolver = context.contentResolver
    val folder = DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree))
    val document = DocumentsContract.createDocument(resolver, folder, typeOf(name), name)
      ?: throw Refusal("The folder ${describe(context, tree)} would not take a new file")
    try {
      val descriptor = resolver.openFileDescriptor(document, "w") ?: throw FileNotFoundException("the new file could not be opened")
      return CreatedDocument(document, descriptor.detachFd(), "${describe(context, tree)}/${displayName(context, document) ?: name}")
    } catch (error: IOException) {
      delete(context, document)
      throw error
    }
  }

  fun delete(context: Context, document: Uri) {
    try {
      DocumentsContract.deleteDocument(context.contentResolver, document)
    } catch (error: FileNotFoundException) {
      Log.w(LOG_TAG, "Could not remove an unfinished file: $error")
    } catch (error: SecurityException) {
      Log.w(LOG_TAG, "Could not remove an unfinished file: $error")
    }
  }

  // The provider picks the name's extension from the type, so the type is taken from the extension to keep the name as it was.
  fun typeOf(name: String): String {
    val extension = name.substringAfterLast('.', "").lowercase()
    return MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension) ?: UNKNOWN_TYPE
  }

  private fun remember(context: Context, tree: Uri) {
    val previous = chosenTree(context)
    context.contentResolver.takePersistableUriPermission(tree, READ_AND_WRITE)
    preferences(context).edit { putString(KEY_TREE, tree.toString()) }
    if (previous != null && previous != tree) release(context, previous)
  }

  private fun release(context: Context, tree: Uri) {
    try {
      context.contentResolver.releasePersistableUriPermission(tree, READ_AND_WRITE)
    } catch (error: SecurityException) {
      Log.w(LOG_TAG, "The old save folder's permission was already gone: $error")
    }
  }

  // A folder whose permission was taken back (its app cleared, the card removed) is forgotten, so files go to LADEX's own storage.
  private fun chosenTree(context: Context): Uri? {
    val saved = preferences(context).getString(KEY_TREE, null) ?: return null
    val tree = saved.toUri()
    val isStillAllowed = context.contentResolver.persistedUriPermissions.any { it.uri == tree && it.isWritePermission }
    if (!isStillAllowed) preferences(context).edit { remove(KEY_TREE) }
    return tree.takeIf { isStillAllowed }
  }

  // On the phone's own storage the folder is named by its path ("Download/LADEX"), as the Files app shows it.
  private fun describe(context: Context, tree: Uri): String {
    val documentId = DocumentsContract.getTreeDocumentId(tree)
    if (tree.authority == EXTERNAL_STORAGE) {
      val path = documentId.substringAfter(':')
      return path.ifEmpty { "the phone's storage" }
    }
    return displayName(context, DocumentsContract.buildDocumentUriUsingTree(tree, documentId)) ?: documentId
  }

  private fun displayName(context: Context, document: Uri): String? {
    val column = DocumentsContract.Document.COLUMN_DISPLAY_NAME
    context.contentResolver.query(document, arrayOf(column), null, null, null)?.use { row ->
      if (row.moveToFirst() && !row.isNull(0)) return row.getString(0)
    }
    return null
  }

  private fun preferences(context: Context) = context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
}
