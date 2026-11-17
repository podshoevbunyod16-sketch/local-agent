package ai.turbollm.app;

import android.app.Application;
import android.util.Log;

public final class TurboApp extends Application {
    private static final String TAG = "TurboLLM";

    @Override
    public void onCreate() {
        super.onCreate();
        try {
            NativeBridge.nativeInit();
            Log.i(TAG, "native backend: " + NativeBridge.nativeBackendInfo());
        } catch (Throwable t) {
            // The APK may legitimately run without the native lib (e.g. an x86
            // emulator image we did not build for) – never crash at startup.
            Log.e(TAG, "native init failed", t);
        }
    }
}
