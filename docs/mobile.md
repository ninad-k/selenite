# Mobile (Android)

The phone app is the same Tauri shell and the same engine as the desktop app, built for Android.
The generated Android Studio project lives in `apps/desktop/src-tauri/gen/android` and is
committed; regenerate it with `cargo tauri android init` only when Tauri asks for it.

## One-time setup (Windows)

1. Install Android Studio (it ships the Java runtime used below).
2. Install the command-line SDK tools, accept the licences, and install the packages:

   ```powershell
   $sdk = "$env:LOCALAPPDATA\Android\Sdk"
   & "$sdk\cmdline-tools\latest\bin\sdkmanager.bat" --licenses
   # sdkmanager.bat splits arguments at ';', so pass the packages in a file:
   Set-Content packages.txt "platforms;android-36`nbuild-tools;37.0.0`nndk;30.0.16248370"
   & "$sdk\cmdline-tools\latest\bin\sdkmanager.bat" --package_file=packages.txt platform-tools
   ```

3. Set the environment (user scope) and add the Rust targets:

   ```powershell
   setx ANDROID_HOME "$env:LOCALAPPDATA\Android\Sdk"
   setx NDK_HOME "$env:LOCALAPPDATA\Android\Sdk\ndk\30.0.16248370"
   setx JAVA_HOME "C:\Program Files\Android\Android Studio\jbr"
   rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
   ```

## Build

```bash
cd apps/desktop && cargo tauri android build --debug --apk --target aarch64
```

`cargo tauri android build` links the native library into the Android project with a symbolic
link, which Windows only allows with Developer Mode on (Settings > System > For developers). Without
it, copy the library and run Gradle directly:

```powershell
cd apps/desktop/src-tauri/gen/android
New-Item -ItemType Directory -Force app\src\main\jniLibs\arm64-v8a
Copy-Item ..\..\..\..\..\target\aarch64-linux-android\debug\libselenite_desktop_lib.so app\src\main\jniLibs\arm64-v8a\
.\gradlew.bat assembleUniversalDebug -x rustBuildUniversalDebug -PtargetList=aarch64 -PabiList=arm64-v8a -ParchList=arm64
```

The debug APK lands under
`apps/desktop/src-tauri/gen/android/app/build/outputs/apk/universal/debug/`. Install it on a phone
with USB debugging on: `adb install -r <apk>`. `cargo tauri android dev` runs it live on a
connected phone or emulator.

## What differs from the desktop

| Area | Mobile behaviour |
|---|---|
| Vault folder | The welcome screen suggests `Vault` inside the app's own data folder (the only place the app may always write). |
| Reminders | Scheduled with the phone's notification system through `tauri-plugin-notification` (`MobileNotifier`), same 7-day window and re-plan on every change. The app asks for notification permission on first start (Android 13+). |
| Assets and plugins | Served at `http://vault.localhost/` and `http://plugin.localhost/`, as on Windows. |
| Cloud sign-in | Not yet usable: the OS-keychain backend has no Android store, so a refresh token would not survive a restart (open question 7). Use a local vault for now. |
| iOS | Needs macOS and Xcode (open question 5). |
