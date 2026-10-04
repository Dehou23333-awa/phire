# Android APK

`.github/workflows/android.yml` builds an installable APK from the repository.

```bash
gh workflow run android.yml --repo <owner>/phire     # or just push; it runs on main and feat/**
```

The artifact `phira-android-apk` contains `phira-android-arm64-v8a.apk`.

## What it takes to build one from scratch

* **Rust**: nothing. `phire-ui` already exports `quad_main()` and the whole
  `Java_quad_1native_QuadNative_*` set, and miniquad provides
  `activityOnCreate`, which is the function that calls `quad_main()`.
* **Java**: only a shell. `android/app/src/main/java/quad_native/QuadNative.java`
  declares the JNI methods, `MainActivity` hosts the `SurfaceView` and forwards
  input plus the activity lifecycle.
* **Assets**: Android reads assets through `AssetManager` using bare names
  (`font.ttf`, not `assets/font.ttf`), so `android/app/build.gradle` points its
  assets source set at the repository's `assets/`. Without this the game never
  reaches its first frame: `the_main()` does `load_file("font.ttf").await?`.
* **ffmpeg**: `prpr-avc` links it, and `prpr-avc/build.rs` looks under
  `static-lib/<TARGET>/`. The workflow pulls the prebuilt libraries from
  `TeamFlos/prpr-avc-ffmpeg` and then verifies that the symbols `prpr-avc`
  declares are really exported, because a mismatch only shows up as an
  `UnsatisfiedLinkError` at launch, never at link time.
* **`libc++_shared.so`**: the `.so` needs it (oboe / C++ dependencies) and
  Gradle does not package it by itself.

## Java methods the native side looks up on the Activity

`phire/src/scene.rs` resolves these with `GetMethodID` and calls the result
*without a null check*, so a missing method is an immediate `SIGSEGV`:

| method | signature | purpose |
|---|---|---|
| `chooseFile` | `()V` | document picker for chart / respack import |
| `inputText` | `(Ljava/lang/String;ZLjava/lang/String;Ljava/lang/String;)V` | text prompt |
| `antiAddiction` | `(Ljava/lang/String;Ljava/lang/String;)V` | only with the `aa` feature |

Both of the first two are called from whichever thread the Rust code runs on,
so they must hop to the UI thread themselves.

## Notes

* arm64-v8a only. NDK 27 no longer ships `libc++_shared.so` for armeabi-v7a,
  and `prpr-avc-ffmpeg` has never been built for `x86_64-linux-android`, so
  there is no emulator target.
* Release builds are signed with the debug key so CI needs no secrets. Replace
  `signingConfig` before publishing.
* No Gradle wrapper is committed (it needs a binary jar); CI runs `gradle`
  directly.
