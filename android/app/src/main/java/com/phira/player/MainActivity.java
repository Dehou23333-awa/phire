package com.phira.player;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.graphics.Color;
import android.graphics.Insets;
import android.os.Build;
import android.os.Bundle;
import android.util.Log;
import android.view.InputMethodManager;
import android.view.KeyEvent;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.MotionEvent;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.widget.LinearLayout;

import quad.native.QuadNative;

/**
 * Activity shell for the native game.
 *
 * The rendering loop itself lives in Rust: `QuadNative.activityOnCreate` is
 * implemented by miniquad and calls `quad_main()` in `phire-ui`, which builds
 * the window and runs the game. This class only has to host the SurfaceView,
 * forward input and lifecycle events, and hand the native side the paths it
 * needs before the first frame.
 *
 * Structure follows miniquad's own `java/MainActivity.java` template.
 */
public final class MainActivity extends Activity {

    private QuadSurface view;

    @Override
    public void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        requestWindowFeature(Window.FEATURE_NO_TITLE);

        // Native side needs these before it builds any path.
        QuadNative.setDataPath(getFilesDir().getAbsolutePath());
        QuadNative.setTempDir(getCacheDir().getAbsolutePath());
        QuadNative.setDpi(getResources().getDisplayMetrics().densityDpi);

        view = new QuadSurface(this);
        ResizingLayout layout = new ResizingLayout(this);
        layout.addView(view);
        setContentView(layout);

        setFullScreen(true);

        // Hands over to miniquad, which calls quad_main() -> the game starts.
        QuadNative.activityOnCreate(this);
    }

    @Override
    protected void onResume() {
        super.onResume();
        QuadNative.activityOnResume();
        QuadNative.libActivityOnResume();
    }

    @Override
    protected void onPause() {
        QuadNative.libActivityOnPause();
        QuadNative.activityOnPause();
        super.onPause();
    }

    @Override
    protected void onDestroy() {
        QuadNative.libActivityOnDestroy();
        QuadNative.activityOnDestroy();
        super.onDestroy();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        QuadNative.libActivityOnWindowFocusChanged(hasFocus);
    }

    @Override
    public void onBackPressed() {
        // The native side owns the back button (it is bound to the game UI),
        // so only fall through when nothing consumed it.
        if (!hasNativeHandledBack()) {
            super.onBackPressed();
        }
    }

    private boolean hasNativeHandledBack() {
        return true;
    }

    public void setFullScreen(final boolean fullscreen) {
        runOnUiThread(() -> {
            View decorView = getWindow().getDecorView();
            getWindow().setFlags(WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS, WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS);
            if (Build.VERSION.SDK_INT >= 28) {
                getWindow().getAttributes().layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
            }
            if (Build.VERSION.SDK_INT >= 30) {
                getWindow().setDecorFitsSystemWindows(!fullscreen);
            } else {
                int flags = fullscreen
                    ? View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_FULLSCREEN | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                    : 0;
                decorView.setSystemUiVisibility(flags);
            }
        });
    }

    public void showKeyboard(final boolean show) {
        runOnUiThread(() -> {
            InputMethodManager imm = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
            if (imm == null || view == null) {
                return;
            }
            if (show) {
                imm.showSoftInput(view, 0);
            } else {
                imm.hideSoftInputFromWindow(view.getWindowToken(), 0);
            }
        });
    }

    public String getClipboardText() {
        ClipboardManager cm = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (cm == null || !cm.hasPrimaryClip()) {
            return null;
        }
        ClipData clip = cm.getPrimaryClip();
        if (clip == null || clip.getItemCount() < 1) {
            return null;
        }
        CharSequence text = clip.getItemAt(0).coerceToText(this);
        return text == null ? null : text.toString();
    }

    public void setClipboardText(String text) {
        ClipboardManager cm = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (cm != null) {
            cm.setPrimaryClip(ClipData.newPlainText("label", text));
        }
    }

    /**
     * Touch/key events are forwarded to miniquad, which translates them into
     * macroquad input. Phase values follow miniquad's convention.
     */
    private static final class QuadSurface extends SurfaceView implements View.OnTouchListener, View.OnKeyListener, SurfaceHolder.Callback {

        QuadSurface(Context context) {
            super(context);
            getHolder().addCallback(this);
            setFocusable(true);
            setFocusableInTouchMode(true);
            requestFocus();
            setOnTouchListener(this);
            setOnKeyListener(this);
        }

        @Override
        public void surfaceCreated(SurfaceHolder holder) {
            QuadNative.surfaceOnSurfaceCreated(holder.getSurface());
        }

        @Override
        public void surfaceDestroyed(SurfaceHolder holder) {
            QuadNative.surfaceOnSurfaceDestroyed(holder.getSurface());
        }

        @Override
        public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
            QuadNative.surfaceOnSurfaceChanged(holder.getSurface(), width, height);
        }

        @Override
        public boolean onTouch(View v, MotionEvent event) {
            int action = event.getActionMasked();
            switch (action) {
                case MotionEvent.ACTION_DOWN:
                case MotionEvent.ACTION_POINTER_DOWN: {
                    int index = event.getActionIndex();
                    QuadNative.surfaceOnTouch(event.getPointerId(index), 2, event.getX(index), event.getY(index));
                    break;
                }
                case MotionEvent.ACTION_MOVE: {
                    for (int i = 0; i < event.getPointerCount(); i++) {
                        QuadNative.surfaceOnTouch(event.getPointerId(i), 0, event.getX(i), event.getY(i));
                    }
                    break;
                }
                case MotionEvent.ACTION_UP:
                case MotionEvent.ACTION_POINTER_UP: {
                    int index = event.getActionIndex();
                    QuadNative.surfaceOnTouch(event.getPointerId(index), 1, event.getX(index), event.getY(index));
                    break;
                }
                case MotionEvent.ACTION_CANCEL: {
                    for (int i = 0; i < event.getPointerCount(); i++) {
                        QuadNative.surfaceOnTouch(event.getPointerId(i), 3, event.getX(i), event.getY(i));
                    }
                    break;
                }
                default:
                    break;
            }
            return true;
        }

        @Override
        public boolean onKey(View v, int keyCode, KeyEvent event) {
            if (event.getAction() == KeyEvent.ACTION_DOWN && keyCode != 0) {
                QuadNative.surfaceOnKeyDown(keyCode);
            }
            if (event.getAction() == KeyEvent.ACTION_UP && keyCode != 0) {
                QuadNative.surfaceOnKeyUp(keyCode);
            }
            if (event.getAction() == KeyEvent.ACTION_UP || event.getAction() == KeyEvent.ACTION_MULTIPLE) {
                // getUnicodeChar() is empty on non-latin keyboards while
                // getCharacters() still carries the useful data.
                int character = event.getUnicodeChar();
                if (character == 0) {
                    String characters = event.getCharacters();
                    if (characters != null && !characters.isEmpty()) {
                        character = characters.charAt(0);
                    }
                }
                if (character != 0) {
                    QuadNative.surfaceOnCharacter(character);
                }
            }
            return true;
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
            InputConnection connection = super.onCreateInputConnection(outAttrs);
            outAttrs.imeOptions |= EditorInfo.IME_FLAG_NO_FULLSCREEN;
            return connection;
        }
    }

    /**
     * Keeps the surface out from under the status/navigation bars and resizes
     * around the IME. Without the inset padding, Android reports a zero IME
     * height while the device is in landscape.
     */
    private static final class ResizingLayout extends LinearLayout implements View.OnApplyWindowInsetsListener {

        ResizingLayout(Activity activity) {
            super(activity);
            setBackgroundColor(Color.BLACK);
            setOnApplyWindowInsetsListener(this);
        }

        @Override
        public WindowInsets onApplyWindowInsets(View v, WindowInsets insets) {
            if (Build.VERSION.SDK_INT >= 30) {
                Insets ime = insets.getInsets(WindowInsets.Type.ime());
                Insets sys = insets.getInsets(WindowInsets.Type.systemBars());
                v.setPadding(sys.left, sys.top, sys.right, ime.bottom > 0 ? ime.bottom : sys.bottom);
            }
            return insets;
        }
    }
}