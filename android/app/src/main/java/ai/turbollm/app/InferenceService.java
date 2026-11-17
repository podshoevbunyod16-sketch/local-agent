package ai.turbollm.app;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.Intent;
import android.os.Build;
import android.os.IBinder;

/**
 * Keeps the process alive (and out of the freezer) while the model is loaded or
 * an answer is being generated with the screen off. The heavy lifting happens on
 * a background thread in {@link MainActivity}; this service only publishes the
 * foreground notification required for that.
 */
public final class InferenceService extends Service {

    private static final String CHANNEL = "turbollm_inference";
    private static final int NOTIFICATION_ID = 42;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String text = intent != null && intent.getStringExtra("text") != null
                ? intent.getStringExtra("text")
                : getString(R.string.notification_title);
        Notification.Builder b;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            NotificationManager nm = getSystemService(NotificationManager.class);
            if (nm.getNotificationChannel(CHANNEL) == null) {
                nm.createNotificationChannel(new NotificationChannel(
                        CHANNEL, getString(R.string.notification_channel),
                        NotificationManager.IMPORTANCE_LOW));
            }
            b = new Notification.Builder(this, CHANNEL);
        } else {
            b = new Notification.Builder(this);
        }
        b.setContentTitle(getString(R.string.app_name))
                .setContentText(text)
                .setSmallIcon(android.R.drawable.stat_sys_download_done)
                .setOngoing(true);
        startForeground(NOTIFICATION_ID, b.build());
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public void onDestroy() {
        stopForeground(true);
        super.onDestroy();
    }
}
