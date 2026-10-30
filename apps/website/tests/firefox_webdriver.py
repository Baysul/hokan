"""Exercise Hokan with an installed Firefox via geckodriver, without Python packages.

Run against a freshly built, running Hokan server. Use --android for an ADB-connected
emulator with Firefox installed; see FIREFOX_TESTING.md for setup and limitations.
"""

import argparse
import json
import os
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path


def run(*args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, **kwargs)


def wait_until(fn, description, seconds=30):
    deadline = time.monotonic() + seconds
    last_error = None
    while time.monotonic() < deadline:
        try:
            result = fn()
            if result:
                return result
        except (RuntimeError, urllib.error.URLError) as exc:
            last_error = exc
        time.sleep(0.2)
    raise RuntimeError(f"Timed out waiting for {description}: {last_error or 'not ready'}")


class WebDriver:
    def __init__(self, origin):
        self.origin = origin
        self.session = None

    def request(self, method, path, payload=None, timeout=40):
        data = None if payload is None else json.dumps(payload).encode()
        request = urllib.request.Request(
            self.origin + path,
            data=data,
            method=method,
            headers={"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                result = json.load(response)
        except urllib.error.HTTPError as exc:
            try:
                message = json.load(exc).get("value", {}).get("message", str(exc))
            except (ValueError, AttributeError):
                message = str(exc)
            raise RuntimeError(message) from exc
        return result["value"]

    def command(self, method, path, payload=None):
        return self.request(method, f"/session/{self.session}{path}", payload)

    def find(self, css):
        result = self.command("POST", "/element", {"using": "css selector", "value": css})
        return result["element-6066-11e4-a52e-4f735466cecf"]

    def text(self, element):
        return self.command("GET", f"/element/{element}/text")

    def property(self, element, name):
        return self.command("GET", f"/element/{element}/property/{name}")

    def click(self, element):
        self.command("POST", f"/element/{element}/click", {})

    def navigate(self, url):
        self.command("POST", "/url", {"url": url})


def adb_device(adb, serial):
    lines = run(adb, "devices").stdout.decode().splitlines()[1:]
    devices = [line.split()[0] for line in lines if line.split()[-1:] == ["device"]]
    if serial:
        if serial not in devices:
            raise RuntimeError(f"ADB device {serial} is not ready")
        return serial
    if len(devices) != 1:
        raise RuntimeError("Connect exactly one Android emulator/device, or pass --serial")
    return devices[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--desktop", action="store_true", help="Test installed desktop Firefox")
    modes.add_argument("--android", action="store_true", help="Test Firefox on an Android device")
    parser.add_argument("--port", type=int, default=3012, help="Running Hokan server port")
    parser.add_argument("--geckodriver", default=shutil.which("geckodriver"), help="Path to geckodriver")
    parser.add_argument("--adb", default=shutil.which("adb"), help="Path to adb (Android mode)")
    parser.add_argument("--serial", help="ADB device serial (Android mode)")
    parser.add_argument("--android-package", default="org.mozilla.firefox")
    parser.add_argument("--allow-android-app-data-wipe", action="store_true", help="Acknowledge that geckodriver clears all data for the Android Firefox app")
    args = parser.parse_args()
    if not args.geckodriver:
        parser.error("geckodriver is required; provide --geckodriver or add it to PATH")
    if args.android and not args.adb:
        parser.error("adb is required in Android mode; provide --adb or add it to PATH")
    if args.android and not args.allow_android_app_data_wipe:
        parser.error("geckodriver clears all Firefox app data on Android; use a disposable emulator and pass --allow-android-app-data-wipe to acknowledge this")

    app_origin = f"http://127.0.0.1:{args.port}"
    with urllib.request.urlopen(app_origin + "/health", timeout=5) as response:
        if response.read() != b"ok":
            raise RuntimeError("The Hokan health endpoint did not return ok")

    serial = adb_device(args.adb, args.serial) if args.android else None
    adb = [args.adb, "-s", serial] if serial else None
    reverse_created = False
    remote_paths = []
    driver_process = None
    driver = None
    with tempfile.TemporaryDirectory(prefix="hokan-firefox-test-") as directory:
        temp_dir = Path(directory)
        filename = f"hokan-test-{uuid.uuid4().hex}.txt"
        expected = b"Hokan Firefox end-to-end test: private file contents\n"
        local_file = temp_dir / filename
        local_file.write_bytes(expected)
        try:
            if adb:
                bindings = run(*adb, "reverse", "--list").stdout.decode().splitlines()
                wanted = f"tcp:{args.port}"
                existing = [line.split() for line in bindings if wanted in line.split()]
                if existing and not any(parts[-2:] == [wanted, wanted] for parts in existing):
                    raise RuntimeError(f"ADB port {args.port} already forwards elsewhere")
                if not existing:
                    run(*adb, "reverse", wanted, wanted)
                    reverse_created = True
                remote_input = f"/sdcard/Documents/{filename}"
                remote_output = f"/sdcard/Download/{filename}"
                remote_paths = [remote_input, remote_output]
                run(*adb, "shell", "mkdir", "-p", "/sdcard/Documents", "/sdcard/Download")
                run(*adb, "push", str(local_file), remote_input)
                file_to_select = remote_input
            else:
                file_to_select = str(local_file)

            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                driver_port = sock.getsockname()[1]
            driver_env = os.environ.copy()
            if args.android:
                driver_env["PATH"] = str(Path(args.adb).resolve().parent) + os.pathsep + driver_env.get("PATH", "")
            driver_process = subprocess.Popen(
                [args.geckodriver, "--port", str(driver_port)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=driver_env,
            )
            driver = WebDriver(f"http://127.0.0.1:{driver_port}")
            wait_until(lambda: driver.request("GET", "/status").get("ready"), "geckodriver")
            options = {"args": ["-headless"], "prefs": {
                "browser.download.folderList": 2,
                "browser.download.dir": directory,
                "browser.download.useDownloadDir": True,
                "browser.helperApps.neverAsk.saveToDisk": "application/octet-stream",
            }}
            if adb:
                options = {
                    "androidPackage": args.android_package,
                    "androidDeviceSerial": serial,
                    "androidIntentArguments": ["-a", "android.intent.action.VIEW", "-d", "about:blank", "--ez", "automationtest", "true"],
                }
            session = driver.request("POST", "/session", {"capabilities": {"alwaysMatch": {"browserName": "firefox", "moz:firefoxOptions": options}}}, timeout=180 if adb else 40)
            driver.session = session["sessionId"]
            print(f"Firefox {session['capabilities'].get('browserVersion', 'unknown')} ({'Android' if adb else 'desktop'})")
            # Android's localhost is forwarded to the desktop server and is a secure context.
            driver.navigate(f"http://localhost:{args.port}/")
            picker = wait_until(lambda: driver.find("#file-input"), "file picker")
            driver.command("POST", f"/element/{picker}/value", {"text": file_to_select})
            submit = driver.find('[data-share-target="submit"]')
            wait_until(lambda: driver.command("GET", f"/element/{submit}/enabled"), "enabled upload button")
            driver.click(submit)
            link = driver.find('[data-share-target="link"]')
            share_url = wait_until(lambda: driver.property(link, "value"), "encrypted share link")
            if not share_url.startswith(f"http://localhost:{args.port}/share/") or "#" not in share_url:
                raise RuntimeError("The share link was missing its ID or key")
            share_id = urllib.parse.urlsplit(share_url).path.rsplit("/", 1)[-1]
            with urllib.request.urlopen(f"{app_origin}/api/files/{share_id}", timeout=5) as response:
                ciphertext = response.read()
            if expected in ciphertext or filename.encode() in ciphertext:
                raise RuntimeError("The server received the file content or filename in plaintext")
            driver.navigate(share_url)
            receive = wait_until(lambda: driver.find('[data-share-target="receiveStatus"]'), "recipient status")
            wait_until(lambda: "Ready to download:" in driver.text(receive), "decryption")
            download = driver.find('[data-share-target="download"]')
            if driver.property(download, "download") != filename:
                raise RuntimeError("The decrypted filename did not match")
            driver.click(download)
            if adb:
                def android_download():
                    result = subprocess.run([*adb, "exec-out", "cat", remote_output], capture_output=True)
                    return result.stdout == expected if result.returncode == 0 else False
                wait_until(android_download, "download to Android Downloads", seconds=45)
            else:
                # The fixture lives in the test directory; Firefox may append a suffix on collision.
                def desktop_download():
                    return any(path != local_file and path.read_bytes() == expected for path in temp_dir.glob("hokan-test-*.txt"))
                wait_until(desktop_download, "downloaded file", seconds=45)
            driver.navigate(share_url.split("#", 1)[0])
            status = wait_until(lambda: driver.find('[data-share-target="receiveStatus"]'), "missing-key status")
            wait_until(lambda: "invalid key" in driver.text(status).lower(), "missing-key error")
            print("PASS: file selection, upload, encrypted link, decrypt, download, missing-key error")
        finally:
            if driver and driver.session:
                try:
                    driver.command("DELETE", "")
                except (RuntimeError, urllib.error.URLError):
                    pass
            if driver_process:
                driver_process.terminate()
                driver_process.wait(timeout=10)
            if adb and remote_paths:
                subprocess.run([*adb, "shell", "rm", "-f", *remote_paths], capture_output=True)
            if adb and reverse_created:
                subprocess.run([*adb, "reverse", "--remove", f"tcp:{args.port}"], capture_output=True)


if __name__ == "__main__":
    main()
