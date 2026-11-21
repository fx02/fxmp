package hr.pvt.fxmp

import android.Manifest
import android.os.Build
import android.os.Bundle
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // The playback notification (and its controls) needs this on Android 13+.
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 0)
    }
  }
}
