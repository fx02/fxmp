package hr.pvt.fxmp

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager

/**
 * Keeps fxmp playing with the screen off: a foreground service (its
 * notification shows the track with previous / play-pause / next) that holds
 * a CPU and a Wi-Fi lock while music plays, so the share stays reachable.
 */
class PlaybackService : Service() {
    companion object {
        private const val CHANNEL_ID = "playback"
        private const val NOTIFICATION_ID = 1
        private const val ACTION_PREVIOUS = "hr.pvt.fxmp.PREVIOUS"
        private const val ACTION_TOGGLE = "hr.pvt.fxmp.TOGGLE"
        private const val ACTION_NEXT = "hr.pvt.fxmp.NEXT"

        @Volatile
        var instance: PlaybackService? = null
    }

    private lateinit var session: MediaSession
    private var cpu: PowerManager.WakeLock? = null
    private var wifi: WifiManager.WifiLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Playback", NotificationManager.IMPORTANCE_LOW),
        )
        session = MediaSession(this, "fxmp").apply {
            setCallback(object : MediaSession.Callback() {
                override fun onPlay() = FxmpPlugin.press("toggle")
                override fun onPause() = FxmpPlugin.press("toggle")
                override fun onSkipToNext() = FxmpPlugin.press("next")
                override fun onSkipToPrevious() = FxmpPlugin.press("previous")
            })
            isActive = true
        }
        instance = this
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_PREVIOUS -> FxmpPlugin.press("previous")
            ACTION_TOGGLE -> FxmpPlugin.press("toggle")
            ACTION_NEXT -> FxmpPlugin.press("next")
            else -> show(
                intent?.getStringExtra("title") ?: "",
                intent?.getStringExtra("artist") ?: "",
                intent?.getBooleanExtra("playing", true) ?: true,
            )
        }
        return START_NOT_STICKY
    }

    /** Shows the track in the notification and holds the locks while playing. */
    fun show(title: String, artist: String, playing: Boolean) {
        val state = if (playing) PlaybackState.STATE_PLAYING else PlaybackState.STATE_PAUSED
        session.setPlaybackState(
            PlaybackState.Builder()
                .setActions(
                    PlaybackState.ACTION_PLAY_PAUSE or PlaybackState.ACTION_PLAY or
                        PlaybackState.ACTION_PAUSE or PlaybackState.ACTION_SKIP_TO_NEXT or
                        PlaybackState.ACTION_SKIP_TO_PREVIOUS,
                )
                .setState(state, PlaybackState.PLAYBACK_POSITION_UNKNOWN, 1f)
                .build(),
        )
        session.setMetadata(
            android.media.MediaMetadata.Builder()
                .putString(android.media.MediaMetadata.METADATA_KEY_TITLE, title)
                .putString(android.media.MediaMetadata.METADATA_KEY_ARTIST, artist)
                .build(),
        )
        val notification = notification(title, artist, playing)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
        if (playing) hold() else release()
    }

    /** Nothing to play any more: the notification goes and the service stops. */
    fun finish() {
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    override fun onDestroy() {
        release()
        session.release()
        instance = null
        super.onDestroy()
    }

    private fun notification(title: String, artist: String, playing: Boolean): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_media_play)
            .setContentTitle(title)
            .setContentText(artist)
            .setContentIntent(open)
            .setOngoing(playing)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .addAction(action(android.R.drawable.ic_media_previous, "Previous", ACTION_PREVIOUS, 1))
            .addAction(
                action(
                    if (playing) android.R.drawable.ic_media_pause else android.R.drawable.ic_media_play,
                    if (playing) "Pause" else "Play",
                    ACTION_TOGGLE,
                    2,
                ),
            )
            .addAction(action(android.R.drawable.ic_media_next, "Next", ACTION_NEXT, 3))
            .setStyle(
                Notification.MediaStyle()
                    .setMediaSession(session.sessionToken)
                    .setShowActionsInCompactView(0, 1, 2),
            )
            .build()
    }

    private fun action(icon: Int, label: String, name: String, code: Int): Notification.Action {
        val intent = PendingIntent.getService(
            this,
            code,
            Intent(this, PlaybackService::class.java).setAction(name),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return Notification.Action.Builder(
            android.graphics.drawable.Icon.createWithResource(this, icon),
            label,
            intent,
        ).build()
    }

    private fun hold() {
        if (cpu == null) {
            cpu = (getSystemService(Context.POWER_SERVICE) as PowerManager)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "fxmp:playback")
                .apply { acquire() }
        }
        if (wifi == null) {
            @Suppress("DEPRECATION")
            wifi = (applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager)
                .createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "fxmp:playback")
                .apply { acquire() }
        }
    }

    private fun release() {
        cpu?.let { if (it.isHeld) it.release() }
        cpu = null
        wifi?.let { if (it.isHeld) it.release() }
        wifi = null
    }
}
