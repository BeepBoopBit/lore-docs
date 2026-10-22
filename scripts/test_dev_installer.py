#!/usr/bin/env python3
"""Exercise installer mutations in temporary directories, with no real Codex writes.

Run: python3 scripts/test_dev_installer.py
Cargo/Lore/Codex fixtures expose failures and record argv. Real Undercurrent MCP
validation is separate because it needs provisioned models and a release build.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
SCRIPT = REPO / "dev.sh"

FAKE_CARGO = r'''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
with open(os.environ["LORE_TEST_LOG"], "a") as log:
    log.write(json.dumps(["cargo", *sys.argv[1:]]) + "\n")
with open(os.environ["LORE_TEST_CWD_LOG"], "a") as log:
    log.write(json.dumps(os.getcwd()) + "\n")
args = sys.argv[1:]
if os.environ.get("LORE_TEST_CARGO_FAIL") == args[0]:
    sys.exit(17)
if args[0] != "build":
    sys.exit(0)
if os.environ.get("LORE_TEST_BUILD_FAIL"):
    sys.exit(4)
profile = "release" if "--release" in args else "debug"
target = pathlib.Path(args[args.index("--target-dir") + 1]) / profile / "lore"
target.parent.mkdir(parents=True, exist_ok=True)
shutil.copy2(os.environ["LORE_TEST_ARTIFACT"], target)
'''

FAKE_LORE = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
if sys.argv[1:] == ["--version"]:
    print("lore 0.1.0-test")
    sys.exit(0)
with open(os.environ["LORE_TEST_LOG"], "a") as log:
    log.write(json.dumps(["lore", *sys.argv[1:]]) + "\n")
if os.environ.get("LORE_TEST_RUN_MODE"):
    print(json.dumps({"args": sys.argv[1:], "cwd": os.getcwd()}))
    sys.exit(23)
if os.environ.get("LORE_TEST_FAIL_STEP") in sys.argv[1:]:
    sys.exit(5)
args = sys.argv[1:]
if args[4:6] == ["model", "install"]:
    pathlib.Path(args[3]).mkdir(parents=True, exist_ok=True)
elif args[4] == "index":
    (pathlib.Path(args[1]) / "index.sqlite").write_text("index state")
print("{}")
'''

FAKE_CODEX = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
with open(os.environ["LORE_TEST_LOG"], "a") as log:
    log.write(json.dumps(["codex", *sys.argv[1:]]) + "\n")
args = sys.argv[1:]
state = pathlib.Path(os.environ["LORE_TEST_CODEX_STATE"])
entries = json.loads(state.read_text()) if state.exists() else {}
if args[1] == "list":
    sys.exit(7 if os.environ.get("LORE_TEST_BAD_CONFIG") else 0)
elif args[1] == "get":
    sys.exit(0 if args[2] in entries else 1)
elif args[1] == "add":
    if os.environ.get("LORE_TEST_CODEX_FAIL"):
        sys.exit(6)
    entries[args[2]] = args
    state.write_text(json.dumps(entries))
'''


def executable(path, text):
    path.write_text(text)
    path.chmod(0o755)


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="lore installer $literal ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "Lore checkout"
        (self.repo / "examples").mkdir(parents=True)
        self.script = self.repo / "dev.sh"
        executable(self.script, SCRIPT.read_text())
        (self.repo / "Cargo.toml").write_text("# test manifest\n")
        (self.repo / "Cargo.lock").write_text("# test lockfile\n")
        (self.repo / "examples/undercurrent-usage.json").write_text('{"roles": []}')
        self.corpus = self.root / "undercurrent"
        self.corpus.mkdir()
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.install = self.root / "installed bin"
        self.binary = self.install / "lore"
        self.artifact = self.root / "artifact"
        self.log = self.root / "calls.jsonl"
        self.cwd_log = self.root / "cargo-cwd.jsonl"
        self.codex_state = self.root / "codex-entries.json"
        executable(self.tools / "cargo", FAKE_CARGO)
        executable(self.tools / "rustc", '#!/usr/bin/env bash\nprintf "rustc %s (test)\\n" "${LORE_TEST_RUST_VERSION:-1.88.0}"\n')
        for compiler in ("cc", "c++"):
            executable(self.tools / compiler, "#!/usr/bin/env bash\nexit 0\n")
        executable(self.tools / "codex", FAKE_CODEX)
        executable(self.artifact, FAKE_LORE)
        self.env = os.environ | {
            "PATH": f"{self.tools}:{os.environ['PATH']}",
            "LORE_DEV_INSTALL_DIR": str(self.install),
            "CARGO_TARGET_DIR": str(self.root / "build cache"),
            "LORE_TEST_ARTIFACT": str(self.artifact),
            "LORE_TEST_LOG": str(self.log),
            "LORE_TEST_CWD_LOG": str(self.cwd_log),
            "LORE_TEST_CODEX_STATE": str(self.codex_state),
        }

    def run_script(self, *args, ok=True):
        result = subprocess.run([str(self.script), *args], cwd=self.root, env=self.env,
                                text=True, capture_output=True, timeout=20)
        if ok:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        return result

    def calls(self, tool=None):
        calls = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return [call for call in calls if tool is None or call[0] == tool]

    def registrations(self):
        return [call for call in self.calls("codex") if call[2] == "add"]

    def test_install_reinstall_and_failed_build_preserve_binary(self):
        self.run_script("install")
        original = self.binary.read_bytes()
        self.run_script("install", "--prod", ok=False)
        self.assertEqual(len(self.calls("cargo")), 1)
        self.env["LORE_TEST_BUILD_FAIL"] = "1"
        self.run_script("reinstall", ok=False)
        self.assertEqual(self.binary.read_bytes(), original)
        del self.env["LORE_TEST_BUILD_FAIL"]
        with self.artifact.open("a") as artifact:
            artifact.write("\n# replacement\n")
        self.run_script("reinstall")
        self.assertNotEqual(self.binary.read_bytes(), original)
        self.assertEqual(list(self.install.iterdir()), [self.binary])
        self.assertEqual(self.calls("codex"), [])

    def test_default_setup_is_independent_of_working_directory(self):
        self.run_script("install", "--prod")
        self.run_script("setup", self.corpus.name)
        call = self.registrations()[0]
        index = self.repo / ".lore/mcp/lore-undercurrent"
        self.assertEqual(call, ["codex", "mcp", "add", "lore-undercurrent", "--",
            str(self.binary), "--index-dir", str(index), "--model-dir",
            str(self.repo / ".lore/models/bge-small"), "mcp"])
        lore_calls = self.calls("lore")
        self.assertEqual(lore_calls[0][-2:], ["model", "install"])
        self.assertEqual(lore_calls[1][-3:], ["index", str(self.corpus), "--json"])
        self.assertEqual(lore_calls[2][-3:], ["status", "--check", "--json"])
        self.assertEqual((index / "index-report.json").read_text(), "{}\n")
        self.assertEqual((index / "status-report.json").read_text(), "{}\n")
        self.assertEqual(self.calls()[-1], call)

    def test_custom_corpus_paths_and_refresh_do_not_inherit_profile(self):
        (self.root / "my docs").mkdir()
        self.run_script("install", "--prod")
        options = ["my docs", "--index-dir", "my state", "--model-dir", "my model"]
        self.run_script("setup", *options, "--name", "lore-other")
        call = self.registrations()[0]
        self.assertEqual(call[3], "lore-other")
        self.assertNotIn("--usage-config", call)
        self.assertEqual(call[7], str(self.root / "my state"))
        self.assertEqual(call[9], str(self.root / "my model"))
        before = self.codex_state.read_bytes()
        self.log.unlink()
        self.run_script("refresh", *options, "--name", "lore-other")
        self.assertEqual([call[0] for call in self.calls()], ["lore", "lore"])
        self.assertNotIn("install", self.calls("lore")[0])
        self.assertEqual(self.codex_state.read_bytes(), before)

    def test_repositories_have_separate_indexes_and_connections(self):
        self.run_script("install", "--prod")
        self.run_script("setup", self.corpus.name)
        (self.root / "my docs").mkdir()
        self.run_script("setup", "my docs")
        entries = json.loads(self.codex_state.read_text())
        self.assertEqual(set(entries), {"lore-undercurrent", "lore-my-docs"})
        indexes = [row[7] for row in self.registrations()]
        self.assertEqual(len(set(indexes)), 2)
        models = [row[9] for row in self.registrations()]
        self.assertEqual(len(set(models)), 1)

    def test_explicit_profile_with_spaces_and_registration_failure(self):
        profile = self.root / "roles config.json"
        profile.write_text('{"roles": []}')
        self.run_script("install", "--prod")
        self.env["LORE_TEST_CODEX_FAIL"] = "1"
        self.run_script("setup", self.corpus.name, "--usage-config", profile.name, ok=False)
        self.assertTrue(self.binary.exists())
        self.assertEqual(self.registrations()[0][-2:], ["--usage-config", str(profile)])
        del self.env["LORE_TEST_CODEX_FAIL"]
        self.run_script("setup", self.corpus.name, "--usage-config", profile.name)
        self.assertEqual(len(self.calls("cargo")), 1)

    def test_failed_provisioning_never_registers(self):
        self.run_script("install", "--prod")
        for step in ("model", "index", "status"):
            with self.subTest(step=step):
                self.env["LORE_TEST_FAIL_STEP"] = step
                self.run_script("setup", self.corpus.name, ok=False)
                self.assertEqual(self.registrations(), [])

    def test_existing_entry_and_invalid_codex_config_stop_before_preparation(self):
        self.run_script("install", "--prod")
        self.codex_state.write_text(json.dumps({"lore-undercurrent": ["keep this connection"]}))
        self.run_script("setup", self.corpus.name, ok=False)
        self.assertEqual(self.calls("lore"), [])
        self.assertFalse((self.repo / ".lore").exists())
        self.assertEqual(json.loads(self.codex_state.read_text())["lore-undercurrent"], ["keep this connection"])
        self.run_script("setup", self.corpus.name, "--replace")
        self.assertEqual(len(self.registrations()), 1)
        self.log.unlink()
        self.env["LORE_TEST_BAD_CONFIG"] = "1"
        self.run_script("setup", self.corpus.name, "--replace", ok=False)
        self.assertEqual(self.calls("lore"), [])
        self.assertEqual(self.registrations(), [])

    def test_failed_refresh_preserves_previous_report_and_cleans_temporary_file(self):
        self.run_script("install", "--prod")
        self.run_script("setup", self.corpus.name)
        index = self.repo / ".lore/mcp/lore-undercurrent"
        previous = (index / "index-report.json").read_bytes()
        self.env["LORE_TEST_FAIL_STEP"] = "index"
        self.run_script("refresh", self.corpus.name, ok=False)
        self.assertEqual((index / "index-report.json").read_bytes(), previous)
        self.assertEqual(list(index.glob("index-report.json.*")), [])

    def test_invalid_built_artifact_does_not_replace_installed_binary(self):
        self.run_script("install", "--prod")
        original = self.binary.read_bytes()
        executable(self.artifact, "#!/usr/bin/env bash\nexit 9\n")
        self.run_script("reinstall", ok=False)
        self.assertEqual(self.binary.read_bytes(), original)

    def test_invalid_options_fail_before_mutations(self):
        for args in [("unknown",), ("install", "--prod", "--bogus"),
                     ("doctor", "--fix"), ("develop", "--update"),
                     ("check", "--fix"), ("fmt", "--check"), ("build", "--bogus"),
                     ("setup",), ("setup", self.corpus.name, "--name", "bad/name"),
                     ("setup", self.corpus.name, "--name", "-flag"),
                     ("setup", self.corpus.name, "--model-dir"),
                     ("setup", self.corpus.name, "--index-dir", "/"),
                     ("refresh", self.corpus.name, "--replace"),
                     ("refresh", self.corpus.name, "--usage-config", "roles.json"),
                     ("uninstall", "--prod"), ("reinstall", "--bogus"),
                     ("install", "--prod", "--root", str(self.root)),
                     ("install", "--prod", "--codex")]:
            with self.subTest(args=args):
                self.run_script(*args, ok=False)
                self.assertFalse(self.install.exists())
                self.assertEqual(self.calls(), [])

    def test_missing_model_refresh_and_missing_profile_fail_before_mutations(self):
        self.run_script("install", "--prod")
        self.run_script("refresh", self.corpus.name, ok=False)
        self.run_script("setup", self.corpus.name, "--usage-config", "missing.json", ok=False)
        self.assertFalse((self.repo / ".lore").exists())
        self.assertEqual(self.calls("lore"), [])
        self.assertEqual(self.calls("codex"), [])

    def test_uninstall_preserves_state_and_never_calls_codex(self):
        self.run_script("install", "--prod")
        state = self.install / "keep-index.sqlite"
        state.write_bytes(b"state")
        self.run_script("uninstall")
        self.run_script("uninstall")
        self.assertFalse(self.binary.exists())
        self.assertEqual(state.read_bytes(), b"state")
        self.assertEqual(self.calls("codex"), [])

    def test_symlink_and_foreign_destinations_are_not_replaced_or_deleted(self):
        self.install.mkdir()
        self.binary.symlink_to(self.artifact)
        original = self.artifact.read_bytes()
        for command in ("reinstall", "uninstall"):
            self.run_script(command, ok=False)
            self.assertTrue(self.binary.is_symlink())
            self.assertEqual(self.artifact.read_bytes(), original)
        self.binary.unlink()
        executable(self.binary, "#!/usr/bin/env bash\nprintf 'another-tool 1.0\\n'\n")
        for command in ("reinstall", "uninstall"):
            self.run_script(command, ok=False)
            self.assertTrue(self.binary.exists())
        self.assertEqual(self.calls("cargo"), [])

    def test_help_does_not_run_tools_or_resolve_install_paths(self):
        self.env["LORE_DEV_INSTALL_DIR"] = "/"
        for args in [(), ("help",), ("--help",), ("-h",),
                     *((command, "--help") for command in
                       ("doctor", "install", "reinstall", "uninstall", "develop",
                        "check", "fmt", "build", "setup", "refresh"))]:
            with self.subTest(args=args):
                result = self.run_script(*args)
                self.assertIn("./dev.sh <command> [args]", result.stdout)
                self.assertIn("develop", result.stdout)
        self.assertEqual(self.calls(), [])
        self.assertFalse(self.install.exists())
        self.assertEqual(self.run_script("unknown", "--help", ok=False).returncode, 2)
        self.assertEqual(self.run_script("check", "--fix", ok=False).returncode, 2)

    def test_development_commands_use_checkout_and_locked_builds(self):
        self.run_script("develop")
        self.run_script("fmt")
        result = self.run_script("build")
        calls = self.calls("cargo")
        self.assertEqual(calls[:2], [["cargo", "fetch", "--locked"],
                                    ["cargo", "fmt", "--all"]])
        self.assertEqual(calls[2], ["cargo", "build", "--manifest-path",
            str(self.repo / "Cargo.toml"), "--locked", "--release", "--bin", "lore",
            "--target-dir", self.env["CARGO_TARGET_DIR"]])
        built = Path(self.env["CARGO_TARGET_DIR"]) / "release/lore"
        self.assertEqual(built.read_bytes(), self.artifact.read_bytes())
        self.assertIn(str(built), result.stdout)
        self.assertEqual([json.loads(line) for line in self.cwd_log.read_text().splitlines()],
                         [str(self.repo)] * 3)
        self.assertFalse(self.install.exists())
        self.assertEqual(self.calls("codex"), [])

    def test_run_preserves_arguments_caller_directory_and_exit_status(self):
        self.env["LORE_TEST_RUN_MODE"] = "1"
        args = ["--index-dir", "state with spaces", "brief", 'café "quoted" $HOME',
                "", "$(not-a-command); & | *", "--help"]
        result = self.run_script("run", *args, ok=False)
        self.assertEqual(result.returncode, 23)
        self.assertEqual(json.loads(result.stdout), {"args": args, "cwd": str(self.root)})
        self.assertEqual(self.calls("lore"), [["lore", *args]])
        self.assertNotIn("--release", self.calls("cargo")[0])
        self.assertEqual(json.loads(self.cwd_log.read_text()), str(self.repo))
        self.assertFalse(self.install.exists())
        self.assertEqual(self.calls("codex"), [])

    def test_run_does_not_launch_after_build_failure(self):
        self.env["LORE_TEST_BUILD_FAIL"] = "1"
        result = self.run_script("run", "--help", ok=False)
        self.assertEqual(result.returncode, 4)
        self.assertEqual(self.calls("lore"), [])

    def test_check_runs_both_checks_and_reports_either_failure(self):
        for failure in ("", "fmt", "clippy"):
            with self.subTest(failure=failure):
                self.log.unlink(missing_ok=True)
                self.env["LORE_TEST_CARGO_FAIL"] = failure
                result = self.run_script("check", ok=not failure)
                self.assertEqual(result.returncode, 17 if failure else 0)
                self.assertEqual(self.calls("cargo"), [
                    ["cargo", "fmt", "--all", "--check"],
                    ["cargo", "clippy", "--all-targets", "--locked", "--target-dir",
                     self.env["CARGO_TARGET_DIR"], "--", "-D", "warnings"],
                ])
        self.assertEqual(set(json.loads(line) for line in self.cwd_log.read_text().splitlines()),
                         {str(self.repo)})

    def test_test_forwards_cargo_arguments_from_checkout(self):
        for args in (["--test", "retrieval", "--", "filter with spaces", "--nocapture"],
                     ["--help"]):
            with self.subTest(args=args):
                self.log.unlink(missing_ok=True)
                self.run_script("test", *args)
                self.assertEqual(self.calls("cargo"), [["cargo", "test", "--locked",
                    "--target-dir", self.env["CARGO_TARGET_DIR"], *args]])
        self.assertEqual(set(json.loads(line) for line in self.cwd_log.read_text().splitlines()),
                         {str(self.repo)})

    def test_default_test_runs_installer_checks_and_preserves_failure_status(self):
        (self.repo / "scripts").mkdir()
        (self.repo / "scripts/test_dev_installer.py").write_text(
            'import json, os, sys\n'
            'with open(os.environ["LORE_TEST_LOG"], "a") as log:\n'
            '    log.write(json.dumps(["installer-tests", os.getcwd()]) + "\\n")\n'
            'sys.exit(int(os.environ.get("LORE_TEST_INSTALLER_FAIL", "0")))\n')
        for cargo_failure, installer_failure, expected in [(False, False, 0),
                                                         (False, True, 22),
                                                         (True, False, 17)]:
            with self.subTest(cargo=cargo_failure, installer=installer_failure):
                self.log.unlink(missing_ok=True)
                self.env["LORE_TEST_CARGO_FAIL"] = "test" if cargo_failure else ""
                self.env["LORE_TEST_INSTALLER_FAIL"] = "22" if installer_failure else "0"
                result = self.run_script("test", ok=expected == 0)
                self.assertEqual(result.returncode, expected)
                self.assertEqual(self.calls("installer-tests"),
                                 [] if cargo_failure else [["installer-tests", str(self.repo)]])

    def test_doctor_is_read_only_and_checks_required_tools(self):
        before = sorted(path.relative_to(self.repo) for path in self.repo.rglob("*"))
        result = self.run_script("doctor")
        self.assertIn("Cargo.lock: found", result.stdout)
        self.assertEqual(self.calls("cargo"), [["cargo", "--version"],
            ["cargo", "fmt", "--version"], ["cargo", "clippy", "--version"]])
        self.assertEqual(self.calls("codex"), [])
        self.assertEqual(before, sorted(path.relative_to(self.repo) for path in self.repo.rglob("*")))
        self.assertFalse(self.install.exists())
        self.env["LORE_TEST_RUST_VERSION"] = "1.87.0"
        self.assertIn("Rust 1.88+ is required", self.run_script("doctor", ok=False).stdout)
        self.env["LORE_TEST_RUST_VERSION"] = "1.88.0"
        self.env["LORE_TEST_CARGO_FAIL"] = "clippy"
        self.assertIn("rustup component add clippy", self.run_script("doctor", ok=False).stdout)
        del self.env["LORE_TEST_CARGO_FAIL"]
        (self.repo / "Cargo.lock").unlink()
        self.assertIn("Cargo.lock: MISSING", self.run_script("doctor", ok=False).stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
