"""cloak — a Fluent-design drive manager (PySide6 + QFluentWidgets).

Minimal by design: create drives, and lock / unlock them. Unlocking mounts the
drive and opens it in your file manager; all file work happens there. The crypto
and storage are the Rust `cloak` engine, reached through the `cloak` CLI (to
verify the password) and the `cloak-winmount` / `cloak-mount` adapter (to mount).
"""

import json
import os
import subprocess
import sys
from pathlib import Path

from PySide6.QtCore import Qt, QTimer
from PySide6.QtGui import QColor
from PySide6.QtWidgets import (
    QApplication, QButtonGroup, QFileDialog, QHBoxLayout, QVBoxLayout, QWidget,
)
from qfluentwidgets import (
    FluentWindow, FluentIcon as FIF, NavigationItemPosition, setTheme, setThemeColor,
    Theme, TitleLabel, SubtitleLabel, BodyLabel, StrongBodyLabel, CaptionLabel,
    PrimaryPushButton, PushButton, TransparentPushButton, LineEdit, PasswordLineEdit,
    ComboBox, CardWidget, MessageBoxBase, FlowLayout, InfoBar, InfoBarPosition,
    RadioButton, SwitchButton,
)

APP_DIR = Path(__file__).resolve().parent
CONFIG = Path.home() / ".cloak" / "drives.json"
IS_WIN = sys.platform.startswith("win")


# ---- engine bridge ---------------------------------------------------------

def _find(exe_base):
    env = os.environ.get("CLOAK_BIN") if exe_base == "cloak" else None
    if env and Path(env).exists():
        return env
    exe = exe_base + (".exe" if IS_WIN else "")
    for rel in ("../target/release", "../target/debug", "."):
        p = APP_DIR / rel / exe
        if p.exists():
            return str(p)
    return exe


CLI = _find("cloak")


def verify(path, password):
    """Open the drive to check the password; raise RuntimeError on failure."""
    env = os.environ.copy()
    env["CLOAK_PASSPHRASE"] = password
    try:
        proc = subprocess.run([CLI, "open", path], capture_output=True, text=True, env=env)
    except OSError as e:
        raise RuntimeError(f"engine not found ({e})")
    out = (proc.stdout or "").strip()
    try:
        data = json.loads(out) if out else {}
    except json.JSONDecodeError:
        data = {}
    if isinstance(data, dict) and "error" in data:
        raise RuntimeError(data["error"])
    if proc.returncode != 0:
        raise RuntimeError((proc.stderr or "could not open drive").strip())


def create_store(path, password):
    env = os.environ.copy()
    env["CLOAK_PASSPHRASE"] = password
    try:
        proc = subprocess.run([CLI, "create", path], capture_output=True, text=True, env=env)
    except OSError as e:
        raise RuntimeError(f"engine not found ({e})")
    out = (proc.stdout or "").strip()
    try:
        data = json.loads(out) if out else {}
    except json.JSONDecodeError:
        data = {}
    if isinstance(data, dict) and "error" in data:
        raise RuntimeError(data["error"])
    if proc.returncode != 0:
        raise RuntimeError((proc.stderr or "could not create drive").strip())


def free_drive_letters():
    if not IS_WIN:
        return []
    return [f"{c}:" for c in "DEFGHIJKLMNOPQRSTUVWXYZ" if not Path(f"{c}\\").exists()]


def load_drives():
    try:
        return json.loads(CONFIG.read_text())
    except Exception:
        return []


def save_drives(drives):
    try:
        CONFIG.parent.mkdir(parents=True, exist_ok=True)
        CONFIG.write_text(json.dumps(drives, indent=2))
    except Exception:
        pass


def open_in_file_manager(mountpoint):
    try:
        if IS_WIN:
            os.startfile(mountpoint if mountpoint.endswith("\\") else mountpoint + "\\")
        elif sys.platform == "darwin":
            subprocess.Popen(["open", mountpoint])
        else:
            subprocess.Popen(["xdg-open", mountpoint])
    except Exception:
        pass


# ---- sleep / shutdown safety ----------------------------------------------
#
# Mounted drives are decrypted and live, so we unmount them when the machine
# sleeps (security) and when it shuts down or logs off (clean teardown). Kept in
# a module global so the filter/observer isn't garbage-collected.

_POWER_HOOK = None


def install_power_hook(app, on_power):
    """Call on_power("sleep") on suspend and on_power("shutdown") on logoff/
    shutdown. Best-effort and silent if the platform hook is unavailable."""
    global _POWER_HOOK
    if IS_WIN:
        import ctypes
        import ctypes.wintypes
        from PySide6.QtCore import QAbstractNativeEventFilter

        WM_POWERBROADCAST, PBT_APMSUSPEND = 0x0218, 0x0004
        WM_QUERYENDSESSION, WM_ENDSESSION = 0x0011, 0x0016

        class _Filter(QAbstractNativeEventFilter):
            def nativeEventFilter(self, _etype, message):
                try:
                    msg = ctypes.wintypes.MSG.from_address(int(message))
                    if msg.message == WM_POWERBROADCAST and msg.wParam == PBT_APMSUSPEND:
                        on_power("sleep")
                    elif msg.message in (WM_QUERYENDSESSION, WM_ENDSESSION):
                        on_power("shutdown")
                except Exception:
                    pass
                return (False, 0)

        _POWER_HOOK = _Filter()
        app.installNativeEventFilter(_POWER_HOOK)
    elif sys.platform == "darwin":
        try:
            from Foundation import NSObject
            from AppKit import NSWorkspace

            class _Sleep(NSObject):
                def onSleep_(self, _note):
                    on_power("sleep")

            obs = _Sleep.new()
            nc = NSWorkspace.sharedWorkspace().notificationCenter()
            nc.addObserver_selector_name_object_(
                obs, "onSleep:", "NSWorkspaceWillSleepNotification", None
            )
            _POWER_HOOK = obs
        except Exception:
            pass


# ---- dialogs ---------------------------------------------------------------

class CreateDialog(MessageBoxBase):
    def __init__(self, parent):
        super().__init__(parent)
        self.titleLabel = SubtitleLabel("Create a new drive", self)
        self.name = LineEdit(self); self.name.setPlaceholderText("Name  (e.g. Personal)")
        self.loc = LineEdit(self); self.loc.setPlaceholderText("Location — an empty folder")
        self.pick = PushButton("Choose folder…", self)
        self.pw = PasswordLineEdit(self); self.pw.setPlaceholderText("Password")
        self.pw2 = PasswordLineEdit(self); self.pw2.setPlaceholderText("Confirm password")
        row = QHBoxLayout(); row.addWidget(self.loc); row.addWidget(self.pick)
        self.viewLayout.addWidget(self.titleLabel)
        self.viewLayout.addWidget(self.name)
        self.viewLayout.addLayout(row)
        self.viewLayout.addWidget(self.pw)
        self.viewLayout.addWidget(self.pw2)
        self.yesButton.setText("Create drive")
        self.cancelButton.setText("Cancel")
        self.widget.setMinimumWidth(430)
        self.pick.clicked.connect(self._choose)

    def _choose(self):
        d = QFileDialog.getExistingDirectory(self, "Choose an empty folder for the drive")
        if d:
            self.loc.setText(d)


class UnlockDialog(MessageBoxBase):
    def __init__(self, parent, name, path, letters):
        super().__init__(parent)
        self.titleLabel = SubtitleLabel(f"Unlock “{name}”", self)
        self.sub = CaptionLabel(path, self)
        self.pw = PasswordLineEdit(self); self.pw.setPlaceholderText("Password")
        self.viewLayout.addWidget(self.titleLabel)
        self.viewLayout.addWidget(self.sub)
        self.viewLayout.addWidget(self.pw)
        self.combo = None
        if letters:
            row = QHBoxLayout()
            row.addWidget(BodyLabel("Mount at", self))
            self.combo = ComboBox(self)
            self.combo.addItems(letters)
            row.addWidget(self.combo); row.addStretch(1)
            self.viewLayout.addLayout(row)
        self.yesButton.setText("Unlock")
        self.cancelButton.setText("Cancel")
        self.widget.setMinimumWidth(390)
        self.pw.returnPressed.connect(self.yesButton.click)


# ---- drive card ------------------------------------------------------------

class DriveCard(CardWidget):
    def __init__(self, drive, mount, callbacks):
        super().__init__()
        self.setFixedSize(260, 158)
        lay = QVBoxLayout(self); lay.setContentsMargins(18, 16, 18, 16); lay.setSpacing(5)

        lay.addWidget(StrongBodyLabel(drive["name"], self))
        path = CaptionLabel(drive["path"], self); path.setWordWrap(True); path.setStyleSheet("color: gray;")
        lay.addWidget(path)
        lay.addStretch(1)

        unlocked = mount is not None
        status = CaptionLabel(
            f"● Unlocked · {mount['mp']}" if unlocked else "○ Locked", self
        )
        status.setStyleSheet("color: #13b3a4;" if unlocked else "color: gray;")
        lay.addWidget(status)

        row = QHBoxLayout()
        if unlocked:
            openb = PushButton("Open", self)
            openb.clicked.connect(lambda: callbacks["open"](drive))
            lockb = PrimaryPushButton("Lock", self)
            lockb.clicked.connect(lambda: callbacks["lock"](drive))
            row.addWidget(openb); row.addStretch(1); row.addWidget(lockb)
        else:
            unlockb = PrimaryPushButton("Unlock", self)
            unlockb.clicked.connect(lambda: callbacks["unlock"](drive))
            removeb = TransparentPushButton("Remove", self)
            removeb.clicked.connect(lambda: callbacks["remove"](drive))
            row.addWidget(unlockb); row.addStretch(1); row.addWidget(removeb)
        lay.addLayout(row)


# ---- drives interface ------------------------------------------------------

class DrivesInterface(QWidget):
    def __init__(self, win):
        super().__init__()
        self.win = win
        self.setObjectName("drivesInterface")
        self.drives = load_drives()
        self.mounts = {}  # path -> {"proc": Popen, "mp": mountpoint}

        lay = QVBoxLayout(self); lay.setContentsMargins(36, 28, 36, 28); lay.setSpacing(16)
        head = QHBoxLayout()
        head.addWidget(TitleLabel("Your drives", self)); head.addStretch(1)
        newbtn = PrimaryPushButton(FIF.ADD, "New drive", self)
        newbtn.clicked.connect(self.create_drive)
        head.addWidget(newbtn)
        lay.addLayout(head)
        lay.addWidget(CaptionLabel("Unlock a drive to mount it — then work with your files in Explorer.", self))

        self.host = QWidget(self)
        self.cards = FlowLayout(self.host, needAni=True)
        self.cards.setHorizontalSpacing(16); self.cards.setVerticalSpacing(16)
        lay.addWidget(self.host)
        lay.addStretch(1)
        self.refresh()

    def refresh(self):
        while self.cards.count():
            item = self.cards.takeAt(0)
            if item:
                item.widget().deleteLater()
        cbs = {"unlock": self.unlock, "lock": self.lock, "open": self.open_drive, "remove": self.remove}
        for d in self.drives:
            self.cards.addWidget(DriveCard(d, self.mounts.get(d["path"]), cbs))

    def err(self, msg):
        InfoBar.error("Error", msg, duration=4500, position=InfoBarPosition.TOP, parent=self.win)

    def ok(self, msg):
        InfoBar.success("Done", msg, duration=2500, position=InfoBarPosition.TOP, parent=self.win)

    def create_drive(self):
        dlg = CreateDialog(self.win)
        if not dlg.exec():
            return
        name, loc, pw, pw2 = dlg.name.text().strip(), dlg.loc.text().strip(), dlg.pw.text(), dlg.pw2.text()
        if not name or not loc or not pw:
            return self.err("Name, location and password are required.")
        if pw != pw2:
            return self.err("Passwords don't match.")
        try:
            create_store(loc, pw)
        except RuntimeError as e:
            return self.err(str(e))
        self.drives = [d for d in self.drives if d["path"] != loc]
        self.drives.insert(0, {"name": name, "path": loc})
        save_drives(self.drives)
        self.refresh()
        self.ok(f"Created “{name}”. Unlock it to mount.")

    def unlock(self, drive):
        letters = free_drive_letters()
        dlg = UnlockDialog(self.win, drive["name"], drive["path"], letters)
        if not dlg.exec():
            return
        pw = dlg.pw.text()
        try:
            verify(drive["path"], pw)
        except RuntimeError as e:
            return self.err(str(e))

        if IS_WIN:
            mp = dlg.combo.currentText() if dlg.combo else "X:"
        else:
            mp = str(Path.home() / "cloak-mnt" / drive["name"])
            Path(mp).mkdir(parents=True, exist_ok=True)

        adapter = _find("cloak-winmount" if IS_WIN else "cloak-mount")
        env = os.environ.copy(); env["CLOAK_PASSPHRASE"] = pw
        try:
            proc = subprocess.Popen([adapter, drive["path"], mp], stdin=subprocess.PIPE, env=env)
        except OSError as e:
            return self.err(f"Couldn't start the mount driver ({e}). Is it installed?")
        self.mounts[drive["path"]] = {"proc": proc, "mp": mp}
        self.refresh()
        self.ok(f"Unlocked at {mp}")
        QTimer.singleShot(1200, lambda: open_in_file_manager(mp))

    def lock(self, drive):
        self._unmount(drive["path"])
        self.refresh()

    def _unmount(self, path):
        m = self.mounts.pop(path, None)
        if not m:
            return
        proc = m["proc"]
        # Closing stdin tells the adapter to unmount cleanly; give it a moment,
        # then force-terminate as a fallback (the driver also unmounts on exit).
        try:
            proc.stdin.close()
        except Exception:
            pass
        try:
            proc.wait(timeout=2)
        except Exception:
            try:
                proc.terminate()
            except Exception:
                pass

    def open_drive(self, drive):
        m = self.mounts.get(drive["path"])
        if m:
            open_in_file_manager(m["mp"])

    def remove(self, drive):
        self.drives = [d for d in self.drives if d["path"] != drive["path"]]
        save_drives(self.drives)
        self.refresh()

    def lock_all(self):
        for path in list(self.mounts.keys()):
            self._unmount(path)


# ---- settings --------------------------------------------------------------

class SettingsInterface(QWidget):
    def __init__(self, win):
        super().__init__()
        self.win = win
        self.setObjectName("settingsInterface")
        lay = QVBoxLayout(self); lay.setContentsMargins(36, 28, 36, 28); lay.setSpacing(12)
        lay.addWidget(TitleLabel("Settings", self))

        lay.addWidget(StrongBodyLabel("Appearance", self))
        lay.addWidget(CaptionLabel("Choose a theme.", self))
        group = QButtonGroup(self)
        for label, theme in (("System", Theme.AUTO), ("Light", Theme.LIGHT), ("Dark", Theme.DARK)):
            rb = RadioButton(label, self)
            rb.clicked.connect(lambda _=False, t=theme: setTheme(t))
            if theme == Theme.AUTO:
                rb.setChecked(True)
            group.addButton(rb)
            lay.addWidget(rb)

        lay.addSpacing(10)
        lay.addWidget(StrongBodyLabel("Security", self))
        row = QHBoxLayout()
        row.addWidget(BodyLabel("Auto-lock drives when the computer sleeps", self))
        row.addStretch(1)
        sw = SwitchButton(self)
        sw.setChecked(win.auto_lock)
        sw.checkedChanged.connect(lambda v: setattr(win, "auto_lock", v))
        row.addWidget(sw)
        lay.addLayout(row)
        lay.addWidget(CaptionLabel("Drives always unmount on shutdown and when you quit.", self))

        lay.addStretch(1)


# ---- window ----------------------------------------------------------------

class Cloak(FluentWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("cloak")
        self.resize(820, 560)
        self.auto_lock = True
        self.drives = DrivesInterface(self)
        self.settings = SettingsInterface(self)
        self.addSubInterface(self.drives, FIF.FOLDER, "Drives")
        self.addSubInterface(self.settings, FIF.SETTING, "Settings", NavigationItemPosition.BOTTOM)

    def on_power(self, kind):
        # Shutdown/logoff always locks (clean teardown); sleep respects the setting.
        if kind == "shutdown" or self.auto_lock:
            self.drives.lock_all()
            self.drives.refresh()

    def closeEvent(self, e):
        self.drives.lock_all()
        super().closeEvent(e)


def main():
    QApplication.setHighDpiScaleFactorRoundingPolicy(
        Qt.HighDpiScaleFactorRoundingPolicy.PassThrough
    )
    app = QApplication(sys.argv)
    setTheme(Theme.AUTO)
    setThemeColor(QColor("#13b3a4"))
    w = Cloak()
    install_power_hook(app, w.on_power)
    app.aboutToQuit.connect(w.drives.lock_all)
    w.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
