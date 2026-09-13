#!/usr/bin/env python3
"""Exercise the real staged-snapshot hook in a disposable, dependency-free crate."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def run(cwd, *args, ok=True):
    env = os.environ.copy()
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        env.pop(key, None)
    result = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True)
    if ok and result.returncode:
        raise AssertionError(f"{args}:\n{result.stdout}\n{result.stderr}")
    return result


def main():
    with tempfile.TemporaryDirectory(prefix="voice hook fixture ") as temp:
        root = Path(temp)
        (root / "src").mkdir()
        (root / "scripts").mkdir()
        (root / ".githooks").mkdir()
        shutil.copy2(ROOT / ".githooks/pre-commit", root / ".githooks/pre-commit")
        # The fixture checks the same Rust commands; it must not recursively test hooks.
        shutil.copy2(ROOT / "scripts/rust-check.sh", root / "scripts/check.sh")
        shutil.copy2(ROOT / "rust-toolchain.toml", root / "rust-toolchain.toml")
        (root / "Cargo.toml").write_text('[package]\nname="hook-fixture"\nversion="0.1.0"\nedition="2024"\n')
        good = "pub fn value() -> u32 {\n    1\n}\n#[test]\nfn behavior() {\n    assert_eq!(value(), 1);\n}\n"
        (root / "src/lib.rs").write_text(good)
        run(root, "git", "init", "-q")
        run(root, "git", "config", "user.name", "Hook Fixture")
        run(root, "git", "config", "user.email", "fixture@example.invalid")
        run(root, "git", "config", "core.hooksPath", ".githooks")
        run(root, "cargo", "generate-lockfile", "--offline")
        run(root, "git", "add", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "src", "scripts", ".githooks")
        run(root, "git", "commit", "-qm", "Valid baseline")

        cases = {
            "format": good.replace("    1", "  1"),
            "lint": good.replace("    1", "    let unit = ();\n    let _ = unit;\n    1"),
            "test": good.replace("    1", "    2"),
        }
        for name, bad in cases.items():
            (root / "src/lib.rs").write_text(bad)
            run(root, "git", "add", "src/lib.rs")
            # A good unstaged version must not hide the bad index.
            (root / "src/lib.rs").write_text(good)
            before = run(root, "git", "diff", "--cached", "--binary").stdout
            result = run(root, "git", "commit", "-qm", f"Must reject {name}", ok=False)
            assert result.returncode != 0, f"Hook accepted {name} failure"
            assert run(root, "git", "diff", "--cached", "--binary").stdout == before
            assert (root / "src/lib.rs").read_text() == good
            run(root, "git", "reset", "--hard", "HEAD")

        # Valid index plus broken worktree succeeds, preserving the unstaged hunk.
        (root / "file with spaces.txt").write_text("staged\n")
        run(root, "git", "add", "file with spaces.txt")
        (root / "file with spaces.txt").write_text("unstaged\n")
        (root / "src/lib.rs").write_text(cases["test"])
        run(root, "git", "commit", "-qm", "Only staged content")
        assert run(root, "git", "show", "HEAD:file with spaces.txt").stdout == "staged\n"
        assert (root / "file with spaces.txt").read_text() == "unstaged\n"
        assert (root / "src/lib.rs").read_text() == cases["test"]
        print("Hook fixtures passed: format, lint, test, spaces, partial staging.")


if __name__ == "__main__":
    main()
