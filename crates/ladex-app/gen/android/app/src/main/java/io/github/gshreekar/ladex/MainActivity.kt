package io.github.gshreekar.ladex

import android.content.Intent
import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import io.github.gshreekar.ladex.android.IncomingShare

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    keepContentAboveKeyboard()
    // An activity Android made again from saved state was handed this intent the first time.
    if (savedInstanceState == null) IncomingShare.receive(this, intent)
  }

  // The app was already running when files were shared into it.
  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    IncomingShare.receive(this, intent)
  }

  // Edge to edge, the window no longer shrinks for the keyboard, so a focused field would hide behind it.
  // The page handles the status and navigation bars itself (CSS safe-area insets); only the keyboard is padded here.
  private fun keepContentAboveKeyboard() {
    ViewCompat.setOnApplyWindowInsetsListener(findViewById(android.R.id.content)) { view, insets ->
      view.setPadding(0, 0, 0, insets.getInsets(WindowInsetsCompat.Type.ime()).bottom)
      insets
    }
  }
}
