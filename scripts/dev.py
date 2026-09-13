#!/usr/bin/env python3
"""Dependency-free development supervisor for macOS and Linux (including WSL2)."""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent


def say(message):
    print(f"[dev] {message}", flush=True)


def toolchain(root):
    match = re.search(r'^channel\s*=\s*"([^"]+)"', (root / "rust-toolchain.toml").read_text(), re.M)
    if not match:
        raise RuntimeError("Missing pinned channel in rust-toolchain.toml")
    return match.group(1)


def system_packages(system=None, which=shutil.which):
    system = system or platform.system()
    if system == "Darwin":
        return [["xcode-select", "--install"]]
    for manager, command in [
        ("apt-get", [["sudo", "apt-get", "update"], ["sudo", "apt-get", "install", "-y", "build-essential", "curl", "ca-certificates", "git", "python3"]]),
        ("dnf", [["sudo", "dnf", "install", "-y", "gcc", "gcc-c++", "make", "curl", "ca-certificates", "git", "python3"]]),
        ("pacman", [["sudo", "pacman", "-S", "--needed", "base-devel", "curl", "ca-certificates", "git", "python"]]),
    ]:
        if which(manager):
            return command
    return []


class Supervisor:
    def __init__(self, root, args):
        self.root, self.args = root, args
        self.child = None
        self.stopping = False
        self.output = None
        self.stdout_file = None
        self.child_group = True
        self.version = toolchain(root)

    def request_stop(self, signum, _frame):
        self.stopping = True

    def stop_child(self):
        child = self.child
        if child is not None and child.poll() is None:
            say(f"Stopping process {child.pid}; waiting for shutdown and trace export...")
            try:
                if self.child_group:
                    os.killpg(child.pid, signal.SIGTERM)
                else:
                    child.terminate()
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                say("Shutdown exceeded 10s; forcing process-group exit. This run may have an incomplete trace.")
                try:
                    if self.child_group:
                        os.killpg(child.pid, signal.SIGKILL)
                    else:
                        child.kill()
                except ProcessLookupError:
                    pass
                child.wait()
        self.child = None
        if self.stdout_file:
            self.stdout_file.close()
            self.stdout_file = None

    def command(self, command, interactive=False):
        if self.stopping:
            return 130
        say(shlex.join(command))
        self.child_group = not interactive
        self.child = subprocess.Popen(command, cwd=self.root, start_new_session=self.child_group)
        while self.child.poll() is None and not self.stopping:
            time.sleep(0.1)
        if self.stopping:
            self.stop_child()
            return 130
        code = self.child.wait()
        self.child = None
        return code

    def doctor(self):
        say(f"Platform: {platform.system()} {platform.machine()}; Python {platform.python_version()}; Rust {self.version}")
        missing = [name for name in ("cc", "git", "curl", "rustup") if not shutil.which(name)]
        for name in missing:
            say(f"Missing: {name}")
        ready = not missing
        if "rustup" not in missing:
            check = subprocess.run(["rustup", "run", self.version, "rustc", "--version"], cwd=self.root, capture_output=True, text=True)
            ready &= check.returncode == 0
            say(check.stdout.strip() if check.returncode == 0 else f"Pinned Rust {self.version} is not installed. Run ./scripts/dev.sh setup")
        if not ready:
            for cmd in system_packages():
                say("System prerequisite: " + shlex.join(cmd))
            say("Then run ./scripts/dev.sh setup (add --install-system to run the listed package installer).")
        return 0 if ready else 1

    def setup(self):
        if self.args.install_system:
            for cmd in system_packages():
                if platform.system() == "Darwin" and subprocess.run(["xcode-select", "-p"], capture_output=True).returncode == 0:
                    continue
                if self.command(cmd, interactive=True):
                    return 1
        if not shutil.which("cc") or not shutil.which("curl") or not shutil.which("git"):
            return self.doctor()
        if not shutil.which("rustup"):
            # Download first; a failed download must never execute a partial installer.
            with tempfile.TemporaryDirectory(prefix="voice-rustup-") as directory:
                installer = str(Path(directory) / "rustup-init.sh")
                if self.command(["curl", "--proto", "=https", "--tlsv1.2", "-fsS", "https://sh.rustup.rs", "-o", installer]):
                    return 1
                if self.command(["sh", installer, "-y", "--profile", "minimal", "--default-toolchain", "none"]):
                    return 1
            add_cargo_path()
        if self.command(["rustup", "toolchain", "install", self.version, "--profile", "minimal", "--component", "rustfmt", "--component", "clippy"]):
            return 1
        if self.command(["rustup", "run", self.version, "cargo", "fetch", "--locked"]):
            return 1
        return self.doctor()

    def snapshot(self):
        paths = [self.root / n for n in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")]
        for folder in ("src", "examples", "tests/fixtures"):
            paths.extend((self.root / folder).rglob("*"))
        if self.args.config:
            paths.append(self.args.config)
        snapshot = {}
        for path in paths:
            try:
                if path.is_file():
                    stat = path.stat()
                    snapshot[str(path)] = (stat.st_mtime_ns, stat.st_size)
            except FileNotFoundError:
                pass  # Atomic editor rename; the next snapshot sees the replacement.
        return snapshot

    def start_run(self):
        if self.stopping:
            return False
        self.version = toolchain(self.root)
        if self.command(["rustup", "run", self.version, "cargo", "build", "--locked", "--all-features"]):
            say("Build failed; fix the code and save to retry.")
            return False
        if self.stopping:
            return False
        target = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
        if not target.is_absolute():
            target = self.root / target
        binary = target / "debug" / "voice-runtime"
        self.output = self.args.output / f"run-{time.time_ns()}"
        self.output.mkdir(parents=True)
        command = [str(binary), "run", "--scenario", self.args.scenario, "--output", str(self.output)]
        if not self.args.virtual:
            command.append("--real-time")
        if self.args.config:
            command += ["--config", str(self.args.config)]
        self.stdout_file = (self.output / "stdout.json").open("w")
        say(f"Running {self.args.scenario} ({'virtual' if self.args.virtual else 'real'} time). Artifacts: {self.output}")
        say("Fake PCM playback is simulated; no speaker output. Ctrl+C closes the session and saves its trace.")
        self.child_group = True
        self.child = subprocess.Popen(command, cwd=self.root, stdout=self.stdout_file, start_new_session=True)
        return True

    def summarize(self):
        if not self.output:
            return
        for path in sorted(self.output.glob("*/lifecycle.json")):
            try:
                lifecycle = json.loads(path.read_text())
                say(f"{path.parent.name}: {lifecycle['close_reason']}; active tasks={lifecycle['active_tasks']}; trace complete={lifecycle['trace_complete']}")
                metrics = json.loads((path.parent / "metrics.json").read_text())
                say(f"Stale chunks received/played: {metrics['stale_chunk_received_count']}/{metrics['stale_chunk_played_count']}")
                for item in metrics["interruptions"]:
                    say(f"Speech onset -> playback stop: {item['interruption_to_playback_stop_ms']}ms")
                for reply in json.loads((path.parent / "playback-truth.json").read_text()):
                    say(f"Generated: {reply['generated_text']}")
                    say(f"Heard:     {reply['heard_text']} ({reply['played_duration_ms']}ms)")
            except (OSError, ValueError, KeyError) as error:
                say(f"Incomplete report at {path.parent}: {error}; inspect the trace and stderr.")

    def run(self, watch):
        observed = self.snapshot()
        started = self.start_run()
        if not started and (not watch or self.stopping):
            return 1
        changed_at = None
        while not self.stopping:
            if self.child is not None and self.child.poll() is not None:
                code = self.child.wait()
                self.stop_child()
                self.summarize()
                if not watch:
                    return code
                say("Watching src/, examples/, fixtures and Cargo files. Save a change to rerun; Ctrl+C exits.")
            if watch:
                current = self.snapshot()
                if current != observed:
                    observed, changed_at = current, time.monotonic()
                if changed_at is not None and time.monotonic() - changed_at >= 0.3:
                    self.stop_child()
                    self.summarize()
                    say("Change detected; previous process joined before rebuilding.")
                    self.start_run()
                    changed_at = None
            time.sleep(0.1)
        self.stop_child()
        self.summarize()
        return 0


def add_cargo_path():
    cargo_bin = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))) / "bin"
    os.environ["PATH"] = str(cargo_bin) + os.pathsep + os.environ.get("PATH", "")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs="?", default="watch", choices=["watch", "demo", "doctor", "setup", "test"])
    parser.add_argument("scenario", nargs="?", default="C", choices=["A", "B", "C", "D", "E", "all"])
    parser.add_argument("--virtual", action="store_true", help="run deterministic virtual time instead of real time")
    parser.add_argument("--config", type=lambda p: Path(p).resolve())
    parser.add_argument("--output", type=lambda p: Path(p).resolve(), default=ROOT / "output" / "dev")
    parser.add_argument("--install-system", action="store_true", help="setup only: run the platform package installer (may require sudo)")
    args = parser.parse_args()
    if sys.version_info < (3, 9) or os.name != "posix":
        parser.error("Use Python 3.9+ on macOS/Linux; on Windows use WSL2.")
    if args.install_system and args.command != "setup":
        parser.error("--install-system requires setup")
    add_cargo_path()
    supervisor = Supervisor(ROOT, args)
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, supervisor.request_stop)
    try:
        if args.command == "setup":
            return supervisor.setup()
        if supervisor.doctor():
            return 1
        if args.command == "doctor":
            return 0
        if args.command == "test":
            return supervisor.command(["sh", "scripts/check.sh"])
        return supervisor.run(watch=args.command == "watch")
    finally:
        supervisor.stop_child()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, ValueError) as error:
        say(str(error))
        sys.exit(1)
