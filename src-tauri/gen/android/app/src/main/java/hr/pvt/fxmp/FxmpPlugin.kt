package hr.pvt.fxmp

import android.app.Activity
import android.content.Intent
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@InvokeArg
class TextArgs {
    lateinit var text: String
}

@InvokeArg
class DataArgs {
    lateinit var data: String
}

@InvokeArg
class ControlsArgs {
    lateinit var channel: Channel
}

@InvokeArg
class PlaybackArgs {
    var title: String? = null
    var artist: String = ""
    var playing: Boolean = false
}

/** The app's Android side, called from Rust (src/mobile). */
@TauriPlugin
class FxmpPlugin(private val activity: Activity) : Plugin(activity) {
    companion object {
        private const val KEY_ALIAS = "fxmp-passwords"
        private const val IV_BYTES = 12

        /** Where the notification's buttons go (Rust's playback controls). */
        @Volatile
        var controls: Channel? = null

        fun press(action: String) {
            val message = JSObject()
            message.put("action", action)
            controls?.send(message)
        }
    }

    /** The AES key in the Android Keystore; created on first use, never exported. */
    private fun key(): SecretKey {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        generator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build(),
        )
        return generator.generateKey()
    }

    @Command
    fun encrypt(invoke: Invoke) {
        try {
            val args = invoke.parseArgs(TextArgs::class.java)
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.ENCRYPT_MODE, key())
            val sealed = cipher.iv + cipher.doFinal(args.text.toByteArray(Charsets.UTF_8))
            val result = JSObject()
            result.put("data", Base64.encodeToString(sealed, Base64.NO_WRAP))
            invoke.resolve(result)
        } catch (e: Exception) {
            invoke.reject(e.toString())
        }
    }

    @Command
    fun decrypt(invoke: Invoke) {
        try {
            val args = invoke.parseArgs(DataArgs::class.java)
            val sealed = Base64.decode(args.data, Base64.NO_WRAP)
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(
                Cipher.DECRYPT_MODE,
                key(),
                GCMParameterSpec(128, sealed, 0, IV_BYTES),
            )
            val plain = cipher.doFinal(sealed, IV_BYTES, sealed.size - IV_BYTES)
            val result = JSObject()
            result.put("text", String(plain, Charsets.UTF_8))
            invoke.resolve(result)
        } catch (e: Exception) {
            invoke.reject(e.toString())
        }
    }

    @Command
    fun setControls(invoke: Invoke) {
        controls = invoke.parseArgs(ControlsArgs::class.java).channel
        invoke.resolve()
    }

    /** Starts, updates or (no title) stops the playback service. */
    @Command
    fun playback(invoke: Invoke) {
        val args = invoke.parseArgs(PlaybackArgs::class.java)
        val context = activity.applicationContext
        Handler(Looper.getMainLooper()).post {
            val running = PlaybackService.instance
            val title = args.title
            when {
                title == null -> running?.finish()
                running != null -> running.show(title, args.artist, args.playing)
                else -> {
                    val intent = Intent(context, PlaybackService::class.java)
                        .putExtra("title", title)
                        .putExtra("artist", args.artist)
                        .putExtra("playing", args.playing)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        context.startForegroundService(intent)
                    } else {
                        context.startService(intent)
                    }
                }
            }
        }
        invoke.resolve()
    }
}
