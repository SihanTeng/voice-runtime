#!/usr/bin/env python3
"""Exercise the actual supervisor with isolated subprocess fixtures, without installing tools."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("voice_dev", ROOT / "scripts/dev.py")
dev = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dev)


class DevTests(unittest.TestCase):
    def test_platform_install_commands_and_pinned_version(self):
        self.assertEqual(dev.toolchain(ROOT), "1.96.1")
        self.assertEqual(dev.system_packages("Darwin"), [["xcode-select", "--install"]])
        for manager in ("apt-get", "dnf", "pacman"):
            commands = dev.system_packages("Linux", lambda name: name == manager)
            self.assertTrue(any(manager in command for command in commands))
        self.assertEqual(dev.system_packages("Linux", lambda _: False), [])

    def test_watch_joins_old_run_recovers_from_build_failure_and_handles_ctrl_c(self):
        with tempfile.TemporaryDirectory(prefix="voice dev spaces ") as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            (root / "src").mkdir()
            (root / "cargo/bin").mkdir(parents=True)
            for name in ("dev.sh", "dev.py"):
                shutil.copy2(ROOT / "scripts" / name, root / "scripts" / name)
            shutil.copyfile(ROOT / "rust-toolchain.toml", root / "rust-toolchain.toml")
            source = root / "src/lib.rs"
            source.write_text("valid source")
            runtime = root / "runtime.py"
            runtime.write_text("#!" + sys.executable + "\n" + '''
import json, os, signal, sys, time
from pathlib import Path
root = Path.cwd()
output = Path(sys.argv[sys.argv.index('--output') + 1]) / 'C'
output.mkdir(parents=True)
active = root / 'active'
if active.exists():
    raise RuntimeError('overlapping runtime processes')
active.write_text(str(os.getpid()))
def stop(signum, frame):
    # Delay the ACK to prove the supervisor waits instead of merely sending TERM.
    time.sleep(0.15)
    (output / 'lifecycle.json').write_text(json.dumps({'close_reason':'sigterm','active_tasks':0,'trace_complete':True}))
    (output / 'metrics.json').write_text(json.dumps({'stale_chunk_received_count':0,'stale_chunk_played_count':0,'interruptions':[]}))
    (output / 'playback-truth.json').write_text('[]')
    active.unlink()
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
(output / 'ready').write_text('ready')
while True: time.sleep(0.05)
''')
            runtime.chmod(0o755)
            rustup = root / "cargo/bin/rustup"
            rustup.write_text("#!" + sys.executable + "\n" + '''
import shutil, sys
from pathlib import Path
root = Path.cwd()
if 'rustc' in sys.argv:
    print('rustc 1.96.1 (fixture)')
elif 'build' in sys.argv:
    if (root / 'active').exists():
        (root / 'overlap').write_text('build before old runtime shutdown')
        sys.exit(3)
    attempts = root / 'attempts'
    attempts.write_text(str(int(attempts.read_text()) + 1) if attempts.exists() else '1')
    if 'BROKEN' in (root / 'src/lib.rs').read_text(): sys.exit(2)
    target = root / 'target/debug'
    target.mkdir(parents=True, exist_ok=True)
    shutil.copy2(root / 'runtime.py', target / 'voice-runtime')
''')
            rustup.chmod(0o755)
            env = dict(os.environ, CARGO_HOME=str(root / "cargo"))
            env.pop("CARGO_TARGET_DIR", None)
            output = root / "results"
            log = root / "dev.log"
            with log.open("w") as stream:
                process = subprocess.Popen([str(root / "scripts/dev.sh"), "watch", "--output", str(output)], cwd="/tmp", env=env, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
                deadline = time.monotonic() + 20

                def wait_for(predicate):
                    while not predicate():
                        self.assertIsNone(process.poll(), log.read_text())
                        self.assertLess(time.monotonic(), deadline, log.read_text())
                        time.sleep(0.03)

                try:
                    wait_for(lambda: len(list(output.glob("*/C/ready"))) == 1)
                    source.write_text("BROKEN source")
                    wait_for(lambda: (root / "attempts").read_text() == "2")
                    wait_for(lambda: len(list(output.glob("*/C/lifecycle.json"))) == 1)
                    self.assertFalse((root / "active").exists())
                    source.write_text("fixed source")
                    wait_for(lambda: len(list(output.glob("*/C/ready"))) == 2)
                    process.send_signal(signal.SIGINT)
                    self.assertEqual(process.wait(timeout=10), 0, log.read_text())
                    self.assertFalse((root / "active").exists())
                    self.assertFalse((root / "overlap").exists())
                    self.assertEqual(len(list(output.glob("*/C/lifecycle.json"))), 2)
                finally:
                    if process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=12)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()
                    # A failed assertion must not leave the isolated fixture child behind.
                    if (root / "active").exists():
                        try:
                            os.kill(int((root / "active").read_text()), signal.SIGTERM)
                        except ProcessLookupError:
                            pass


if __name__ == "__main__":
    unittest.main()
