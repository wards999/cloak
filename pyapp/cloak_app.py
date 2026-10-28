"""cloak — a Fluent-design drive manager (PySide6 + QFluentWidgets).

The polished front end; all crypto/storage is the Rust `cloak` engine, reached
through the `cloak` CLI (one JSON line per command, password via the
CLOAK_PASSPHRASE env var). Mounting shells out to cloak-winmount / cloak-mount.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

from PySide6.QtCore import Qt
from PySide6.QtGui import QColor
from PySide6.QtWidgets import (
    QApplication, QFileDialog, QFrame, QHBoxLayout, QStackedWidget, QVBoxLayout,
    QWidget, QHeaderView, QTableWidgetItem, QButtonGroup,
)
from qfluentwidgets import (
    FluentWindow, FluentIcon as FIF, NavigationItemPosition, setTheme, setThemeColor,
    Theme, TitleLabel, SubtitleLabel, BodyLabel, StrongBodyLabel, CaptionLabel,
    PrimaryPushButton, PushButton, TransparentPushButton, LineEdit, PasswordLineEdit,
    ComboBox, CardWidget, MessageBoxBase, FlowLayout, TableWidget, InfoBar,
    InfoBarPosition, RadioButton,
)

APP_DIR = Path(__file__).resolve().parent
CONFIG = Path.home() / ".cloak" / "drives.json"
IS_WIN = sys.platform.startswith("win")


# ---- engine bridge ---------------------------------------------------------

def _find(exe_base: str) -> str:
    env = os.environ.get("CLOAK_BIN") if exe_base == "cloak" else None
    if env and Path(env).exists():
        return env
    exe = exe_base + (".exe" if IS_WIN else "")
    for rel in ("../target/release", "../target/debug", "."):
        p = APP_DIR / rel / exe
        if p.exists():
            return str(p)
    return exe  # fall back to PATH


CLI = _find("cloak")


def cli(args, password):
    env = os.environ.copy()
    env["CLOAK_PASSPHRASE"] = password
    proc = subprocess.run([CLI, *args], capture_output=True, text=True, env=env)
    out = (proc.stdout or "").strip()
    data = None
    try:
        data = json.loads(out) if out else {}
    except json.JSONDecodeError:
        data = {}
    if isinstance(data, dict) and "error" in data:
        raise RuntimeError(data["error"])
    if proc.returncode != 0:
        raise RuntimeError((proc.stderr or "command failed").strip())
    return data


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


def human(n):
    v = float(n)
    for u in ("B", "KB", "MB", "GB", "TB"):
        if v < 1024 or u == "TB":
            return f"{int(n)} B" if u == "B" else f"{v:.1f} {u}"
        v /= 1024


def join(d, name):
    return name if not d else f"{d}/{name}"


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
        for w in (self.titleLabel, self.name):
            self.viewLayout.addWidget(w)
        self.viewLayout.addLayout(row)
        self.viewLayout.addWidget(self.pw)
        self.viewLayout.addWidget(self.pw2)
        self.yesButton.setText("Create drive")
        self.cancelButton.setText("Cancel")
        self.widget.setMinimumWidth(420)
        self.pick.clicked.connect(self._choose)

    def _choose(self):
        d = QFileDialog.getExistingDirectory(self, "Choose an empty folder for the drive")
        if d:
            self.loc.setText(d)


class UnlockDialog(MessageBoxBase):
    def __init__(self, parent, name, path):
        super().__init__(parent)
        self.titleLabel = SubtitleLabel(f"Unlock “{name}”", self)
        self.sub = CaptionLabel(path, self)
        self.pw = PasswordLineEdit(self); self.pw.setPlaceholderText("Password")
        self.viewLayout.addWidget(self.titleLabel)
        self.viewLayout.addWidget(self.sub)
        self.viewLayout.addWidget(self.pw)
        self.yesButton.setText("Unlock")
        self.cancelButton.setText("Cancel")
        self.widget.setMinimumWidth(380)
        self.pw.returnPressed.connect(self.yesButton.click)


class NameDialog(MessageBoxBase):
    def __init__(self, parent, title, placeholder):
        super().__init__(parent)
        self.titleLabel = SubtitleLabel(title, self)
        self.edit = LineEdit(self); self.edit.setPlaceholderText(placeholder)
        self.viewLayout.addWidget(self.titleLabel)
        self.viewLayout.addWidget(self.edit)
        self.yesButton.setText("OK")
        self.cancelButton.setText("Cancel")
        self.widget.setMinimumWidth(360)
        self.edit.returnPressed.connect(self.yesButton.click)


# ---- drive card ------------------------------------------------------------

class DriveCard(CardWidget):
    def __init__(self, drive, on_unlock, on_remove):
        super().__init__()
        self.setFixedSize(250, 150)
        lay = QVBoxLayout(self); lay.setContentsMargins(18, 16, 18, 16); lay.setSpacing(6)
        name = StrongBodyLabel(drive["name"], self)
        path = CaptionLabel(drive["path"], self); path.setWordWrap(True)
        path.setStyleSheet("color: gray;")
        lay.addWidget(name)
        lay.addWidget(path)
        lay.addStretch(1)
        row = QHBoxLayout()
        unlock = PrimaryPushButton("Unlock", self)
        unlock.clicked.connect(lambda: on_unlock(drive))
        remove = TransparentPushButton("Remove", self)
        remove.clicked.connect(lambda: on_remove(drive))
        row.addWidget(unlock); row.addStretch(1); row.addWidget(remove)
        lay.addLayout(row)


# ---- the Drives interface (list + browser) ---------------------------------

class DrivesInterface(QWidget):
    def __init__(self, win):
        super().__init__()
        self.win = win
        self.setObjectName("drivesInterface")
        self.drives = load_drives()
        self.cur = None       # open drive dict
        self.password = ""
        self.cwd = ""
        self.mount_proc = None

        self.stack = QStackedWidget(self)
        outer = QVBoxLayout(self); outer.setContentsMargins(0, 0, 0, 0)
        outer.addWidget(self.stack)

        self.stack.addWidget(self._build_list())
        self.stack.addWidget(self._build_browser())
        self.refresh_list()

    # ----- list page -----
    def _build_list(self):
        page = QWidget(); page.setObjectName("listPage")
        lay = QVBoxLayout(page); lay.setContentsMargins(36, 28, 36, 28); lay.setSpacing(16)
        head = QHBoxLayout()
        title = TitleLabel("Your drives", page)
        newbtn = PrimaryPushButton(FIF.ADD, "New drive", page)
        newbtn.clicked.connect(self.create_drive)
        head.addWidget(title); head.addStretch(1); head.addWidget(newbtn)
        lay.addLayout(head)
        lay.addWidget(CaptionLabel("Unlock a drive with its password, or create a new one.", page))

        self.cards_host = QWidget(page)
        self.cards = FlowLayout(self.cards_host, needAni=True)
        self.cards.setHorizontalSpacing(16); self.cards.setVerticalSpacing(16)
        lay.addWidget(self.cards_host)
        lay.addStretch(1)
        return page

    def refresh_list(self):
        while self.cards.count():
            item = self.cards.takeAt(0)
            if item:
                item.widget().deleteLater()
        for d in self.drives:
            self.cards.addWidget(DriveCard(d, self.unlock_drive, self.remove_drive))

    # ----- browser page -----
    def _build_browser(self):
        page = QWidget(); page.setObjectName("browserPage")
        lay = QVBoxLayout(page); lay.setContentsMargins(36, 24, 36, 24); lay.setSpacing(14)

        bar = QHBoxLayout()
        self.dvName = SubtitleLabel("", page)
        lock = PushButton(FIF.RETURN, "Lock drive", page) if hasattr(FIF, "RETURN") else PushButton("Lock drive", page)
        lock.clicked.connect(self.lock_drive)
        bar.addWidget(self.dvName); bar.addStretch(1); bar.addWidget(lock)
        lay.addLayout(bar)
        self.dvPath = CaptionLabel("", page); self.dvPath.setStyleSheet("color: gray;")
        lay.addWidget(self.dvPath)

        tools = QHBoxLayout()
        up = PushButton("Up", page); up.clicked.connect(self.nav_up)
        imp = PrimaryPushButton(FIF.ADD, "Import file", page); imp.clicked.connect(self.import_file)
        mkd = PushButton(FIF.FOLDER, "New folder", page); mkd.clicked.connect(self.make_folder)
        self.crumb = CaptionLabel("/", page)
        tools.addWidget(up); tools.addWidget(imp); tools.addWidget(mkd)
        tools.addStretch(1); tools.addWidget(self.crumb)
        lay.addLayout(tools)

        self.table = TableWidget(page)
        self.table.setColumnCount(2)
        self.table.setHorizontalHeaderLabels(["Name", "Size"])
        self.table.verticalHeader().hide()
        self.table.setSelectionBehavior(self.table.SelectionBehavior.SelectRows)
        self.table.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        self.table.horizontalHeader().setSectionResizeMode(1, QHeaderView.ResizeMode.ResizeToContents)
        self.table.cellDoubleClicked.connect(self._row_activated)
        lay.addWidget(self.table, 1)

        acts = QHBoxLayout()
        ext = PushButton("Extract…", page); ext.clicked.connect(self.extract_sel)
        rn = PushButton("Rename…", page); rn.clicked.connect(self.rename_sel)
        dl = PushButton("Delete", page); dl.clicked.connect(self.delete_sel)
        acts.addWidget(ext); acts.addWidget(rn); acts.addWidget(dl); acts.addStretch(1)
        lay.addLayout(acts)

        mount = CardWidget(page)
        ml = QHBoxLayout(mount); ml.setContentsMargins(16, 12, 16, 12)
        ml.addWidget(BodyLabel("Mount as a drive", mount))
        self.mountSel = ComboBox(mount)
        self.mountSel.addItems(["X:", "Y:", "Z:", "W:"] if IS_WIN else ["~/cloak-mnt"])
        self.mountBtn = PrimaryPushButton("Mount", mount); self.mountBtn.clicked.connect(self.toggle_mount)
        self.mountMsg = CaptionLabel("", mount)
        ml.addWidget(self.mountSel); ml.addWidget(self.mountBtn); ml.addWidget(self.mountMsg); ml.addStretch(1)
        lay.addWidget(mount)
        return page

    # ----- actions -----
    def toast_err(self, msg):
        InfoBar.error("Error", msg, duration=4000, position=InfoBarPosition.TOP, parent=self)

    def toast_ok(self, msg):
        InfoBar.success("Done", msg, duration=2000, position=InfoBarPosition.TOP, parent=self)

    def create_drive(self):
        dlg = CreateDialog(self.win)
        if not dlg.exec():
            return
        name, loc = dlg.name.text().strip(), dlg.loc.text().strip()
        pw, pw2 = dlg.pw.text(), dlg.pw2.text()
        if not name or not loc or not pw:
            return self.toast_err("Name, location and password are required.")
        if pw != pw2:
            return self.toast_err("Passwords don't match.")
        try:
            cli(["create", loc], pw)
        except RuntimeError as e:
            return self.toast_err(str(e))
        self.drives = [d for d in self.drives if d["path"] != loc]
        self.drives.insert(0, {"name": name, "path": loc})
        save_drives(self.drives)
        self.refresh_list()
        self._open(self.drives[0], pw)

    def unlock_drive(self, drive):
        dlg = UnlockDialog(self.win, drive["name"], drive["path"])
        if not dlg.exec():
            return
        pw = dlg.pw.text()
        try:
            cli(["open", drive["path"]], pw)
        except RuntimeError as e:
            return self.toast_err(str(e))
        self._open(drive, pw)

    def remove_drive(self, drive):
        self.drives = [d for d in self.drives if d["path"] != drive["path"]]
        save_drives(self.drives)
        self.refresh_list()

    def _open(self, drive, pw):
        self.cur, self.password, self.cwd = drive, pw, ""
        self.dvName.setText(drive["name"])
        self.dvPath.setText(drive["path"])
        self.mountMsg.setText("")
        self.stack.setCurrentIndex(1)
        self.refresh_browser()

    def lock_drive(self):
        self._stop_mount()
        self.cur, self.password, self.cwd = None, "", ""
        self.stack.setCurrentIndex(0)

    def refresh_browser(self):
        self.crumb.setText("/" + self.cwd)
        try:
            rows = cli(["ls", self.cur["path"], self.cwd], self.password)
        except RuntimeError as e:
            return self.toast_err(str(e))
        rows.sort(key=lambda r: (not r["is_dir"], r["name"].lower()))
        self.table.setRowCount(len(rows))
        self._rows = rows
        for i, r in enumerate(rows):
            nm = QTableWidgetItem(("🗀  " if r["is_dir"] else "🗋  ") + r["name"])
            nm.setData(Qt.ItemDataRole.UserRole, r)
            self.table.setItem(i, 0, nm)
            self.table.setItem(i, 1, QTableWidgetItem("" if r["is_dir"] else human(r["size"])))

    def _row_activated(self, row, _col):
        r = self._rows[row]
        if r["is_dir"]:
            self.cwd = join(self.cwd, r["name"])
            self.refresh_browser()

    def _selected(self):
        row = self.table.currentRow()
        if row < 0 or row >= len(self._rows):
            return None
        return self._rows[row]

    def nav_up(self):
        if self.cwd:
            self.cwd = self.cwd.rsplit("/", 1)[0] if "/" in self.cwd else ""
            self.refresh_browser()

    def import_file(self):
        src, _ = QFileDialog.getOpenFileName(self, "Import a file")
        if not src:
            return
        name = Path(src).name
        try:
            cli(["write", self.cur["path"], join(self.cwd, name), src], self.password)
            self.toast_ok(f"Imported {name}")
        except RuntimeError as e:
            return self.toast_err(str(e))
        self.refresh_browser()

    def make_folder(self):
        dlg = NameDialog(self.win, "New folder", "Folder name")
        if not dlg.exec():
            return
        name = dlg.edit.text().strip()
        if not name:
            return
        try:
            cli(["mkdir", self.cur["path"], join(self.cwd, name)], self.password)
        except RuntimeError as e:
            return self.toast_err(str(e))
        self.refresh_browser()

    def extract_sel(self):
        r = self._selected()
        if not r or r["is_dir"]:
            return
        dest, _ = QFileDialog.getSaveFileName(self, "Extract to", r["name"])
        if not dest:
            return
        try:
            cli(["read", self.cur["path"], join(self.cwd, r["name"]), dest], self.password)
            self.toast_ok("Extracted.")
        except RuntimeError as e:
            self.toast_err(str(e))

    def rename_sel(self):
        r = self._selected()
        if not r:
            return
        dlg = NameDialog(self.win, "Rename", "New name")
        dlg.edit.setText(r["name"])
        if not dlg.exec():
            return
        new = dlg.edit.text().strip()
        if not new:
            return
        try:
            cli(["mv", self.cur["path"], join(self.cwd, r["name"]), join(self.cwd, new)], self.password)
        except RuntimeError as e:
            return self.toast_err(str(e))
        self.refresh_browser()

    def delete_sel(self):
        r = self._selected()
        if not r:
            return
        try:
            cli(["rm", self.cur["path"], join(self.cwd, r["name"])], self.password)
        except RuntimeError as e:
            return self.toast_err(str(e))
        self.refresh_browser()

    # ----- mount -----
    def toggle_mount(self):
        if self.mount_proc:
            self._stop_mount()
            return
        exe = _find("cloak-winmount" if IS_WIN else "cloak-mount")
        env = os.environ.copy(); env["CLOAK_PASSPHRASE"] = self.password
        try:
            self.mount_proc = subprocess.Popen(
                [exe, self.cur["path"], self.mountSel.currentText()],
                stdin=subprocess.PIPE, env=env,
            )
            self.mountBtn.setText("Unmount")
            self.mountSel.setDisabled(True)
            self.mountMsg.setText(f"Mounted at {self.mountSel.currentText()}")
        except OSError as e:
            self.toast_err(f"Couldn't launch mount adapter: {e}")

    def _stop_mount(self):
        if self.mount_proc:
            try:
                self.mount_proc.stdin.close()
            except Exception:
                pass
            try:
                self.mount_proc.terminate()
            except Exception:
                pass
            self.mount_proc = None
            self.mountBtn.setText("Mount")
            self.mountSel.setDisabled(False)
            self.mountMsg.setText("")


# ---- settings (theme) ------------------------------------------------------

class SettingsInterface(QWidget):
    def __init__(self):
        super().__init__()
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
        lay.addStretch(1)


# ---- window ----------------------------------------------------------------

class Cloak(FluentWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("cloak")
        self.resize(900, 620)
        self.drives = DrivesInterface(self)
        self.settings = SettingsInterface()
        self.addSubInterface(self.drives, FIF.FOLDER, "Drives")
        self.addSubInterface(self.settings, FIF.SETTING, "Settings", NavigationItemPosition.BOTTOM)

    def closeEvent(self, e):
        self.drives._stop_mount()
        super().closeEvent(e)


def main():
    QApplication.setHighDpiScaleFactorRoundingPolicy(
        Qt.HighDpiScaleFactorRoundingPolicy.PassThrough
    )
    app = QApplication(sys.argv)
    setTheme(Theme.AUTO)
    setThemeColor(QColor("#13b3a4"))  # cloak teal accent
    w = Cloak()
    w.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
