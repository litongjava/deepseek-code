package com.deepseek.code

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder

/**
 * Keeps the app process at foreground priority while the engine runs, so a
 * task the model is working on continues when the app is in the background.
 * The engine itself runs inside the app process; this service only holds it.
 */
class EngineService : Service() {
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val notification = buildNotification()
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
    } else {
      startForeground(NOTIFICATION_ID, notification)
    }
    return START_STICKY
  }

  // Closing the app from recent apps ends the engine as the user expects.
  override fun onTaskRemoved(rootIntent: Intent?) {
    stopSelf()
    super.onTaskRemoved(rootIntent)
  }

  private fun buildNotification(): Notification {
    val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      manager.createNotificationChannel(
        NotificationChannel(CHANNEL_ID, getString(R.string.engine_channel_name), NotificationManager.IMPORTANCE_LOW)
      )
    }
    val open = PendingIntent.getActivity(
      this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE
    )
    val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) Notification.Builder(this, CHANNEL_ID)
      else @Suppress("DEPRECATION") Notification.Builder(this)
    return builder
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(getString(R.string.app_name))
      .setContentText(getString(R.string.engine_running))
      .setContentIntent(open)
      .setOngoing(true)
      .build()
  }

  companion object {
    private const val CHANNEL_ID = "engine"
    private const val NOTIFICATION_ID = 1

    fun start(context: Context) {
      val intent = Intent(context, EngineService::class.java)
      try {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) context.startForegroundService(intent)
        else context.startService(intent)
      } catch (error: Exception) {
        // Some devices refuse foreground services; the app still works in the foreground.
        android.util.Log.w("EngineService", "Cannot start foreground service", error)
      }
    }
  }
}
