package quad_native;

import android.view.Surface;

/**
 * JNI bridge. Each declaration here must match a `Java_quad_1native_QuadNative_*`
 * symbol in the `libphire_ui.so` built from `phire-ui`.
 *
 * The `activityOn*` / `surfaceOn*` entry points come from miniquad
 * (miniquad/src/native/android.rs). Everything else is exported by phire-ui
 * itself (phire-ui/src/lib.rs, `#[cfg(target_os = "android")]`).
 */
public final class QuadNative {
    static {
        System.loadLibrary("phire_ui");
    }

    private QuadNative() {}

    // ---- miniquad ----

    public static native void activityOnCreate(Object activity);

    public static native void activityOnResume();

    public static native void activityOnPause();

    public static native void activityOnDestroy();

    public static native void surfaceOnSurfaceCreated(Surface surface);

    public static native void surfaceOnSurfaceDestroyed(Surface surface);

    public static native void surfaceOnTouch(int id, int phase, float x, float y);

    public static native void surfaceOnSurfaceChanged(Surface surface, int width, int height);

    public static native void surfaceOnKeyDown(int keycode);

    public static native void surfaceOnKeyUp(int keycode);

    public static native void surfaceOnCharacter(int character);

    // ---- phire-ui: activity lifecycle (distinct from miniquad's, both are called) ----

    public static native void libActivityOnPause();

    public static native void libActivityOnResume();

    public static native void libActivityOnWindowFocusChanged(boolean hasFocus);

    public static native void libActivityOnDestroy();

    // ---- phire-ui: environment ----

    /** App files dir. The game stores `data.json`, `data/charts/*`, `data/respack` under it. */
    public static native void setDataPath(String path);

    public static native void setTempDir(String path);

    public static native void setDpi(int dpi);

    // ---- phire-ui: results pushed from the native side ----

    public static native void setChosenFile(String file);

    public static native void setInputText(String text);

    /** Non-null requests the native side to open a chart / respack import flow. */
    public static native void markImport();

    public static native void markImportRespack();

    // ---- phire-ui: misc callbacks ----

    public static native void antiAddictionCallback(int code);

    public static native void updateGyroScope(float x, float y, float z, long timestamp);

    public static native void updateGravity(float roll, float pitch, float yaw);
}