# Firefox integration checks

`firefox_webdriver.py` runs the installed desktop Firefox or Firefox for Android
through Mozilla geckodriver. It uses only Python's standard library. The test
creates a disposable file, selects it in the page, uploads it, checks that the
server response does not contain the plaintext or filename, opens the generated
share link, downloads the decrypted file, checks its bytes, and checks a missing
key error. A passing mobile viewport test in desktop Firefox is not a substitute
for running this against the actual Android app.

Install Python 3 and [Mozilla geckodriver](https://github.com/mozilla/geckodriver/releases)
(0.37.1 or newer recommended) and put `geckodriver` on PATH, or pass
`--geckodriver` with its full path. Build and run the current Hokan server first:

```powershell
$env:Path = "$PWD/node_modules/.bin;$env:Path"
vp run website#build
$env:HOKAN_ADDRESS = "127.0.0.1:3012"
cargo run --manifest-path apps/server/Cargo.toml
```

In a second terminal, from the repository root:

```powershell
python apps/website/tests/firefox_webdriver.py --desktop --port 3012
```

Desktop mode uses installed Firefox in a temporary headless profile; it does not
use or alter the normal browser profile. To test Android, start an Android Studio
emulator, install the current Firefox app, and ensure the Android SDK's
`platform-tools` directory is on PATH. Check `adb devices` shows the emulator as
`device`, then run:

**Android WebDriver clears all data for the target Firefox app when it starts.**
This deletes its tabs, browsing history, bookmarks, settings, cookies, and saved
credentials. Use a disposable emulator and only run this after acknowledging the
data loss. This does not affect desktop Firefox.

```powershell
python apps/website/tests/firefox_webdriver.py --android --port 3012 --allow-android-app-data-wipe
```

Use `--adb <path>` or `--serial <device-serial>` when needed. The default Android
package is `org.mozilla.firefox`; use `--android-package` for another channel.
The script forwards the port with `adb reverse`, uses `localhost` on the device
so Web Crypto has a secure context, and removes its own forwarding and temporary
files afterward. If the port is already forwarded elsewhere, it stops rather than
overwriting that mapping. Android mode checks that the file actually arrives in
the device's Downloads folder; a system download prompt, unsupported WebDriver
file selection, or a different download location may need manual investigation.

Mozilla documents an onboarding-related automation hang in Firefox Android 153+
and recommends the intent arguments used in this script. See the
[geckodriver release notes](https://github.com/mozilla/geckodriver/releases) and
[Firefox Android capabilities](https://developer.mozilla.org/en-US/docs/Web/WebDriver/Reference/Capabilities/firefoxOptions).
This test covers one browser version and device configuration; it does not prove
compatibility across every Firefox release or Android device.
