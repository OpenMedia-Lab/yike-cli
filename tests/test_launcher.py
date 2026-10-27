import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import uuid

sys.dont_write_bytecode = True
SCRIPT = Path(__file__).resolve().parents[1] / "scripts/run-backup.py"
spec = importlib.util.spec_from_file_location("backup_launcher", SCRIPT)
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)


@unittest.skipIf(os.name == "nt", "optional POSIX launcher; Windows uses native Rust CLI")
class LauncherTests(unittest.TestCase):
    def make_recording_cli(self, directory):
        fake = directory / "fake-yike"
        calls = directory / "calls.jsonl"
        fake.write_text(f"#!{sys.executable}\n" + f"calls = {str(calls)!r}\n" + """
import sys,json
args=sys.argv[1:]
with open(calls,'a') as stream: stream.write(json.dumps(args)+'\\n')
if 'doctor' in args: print('{}');sys.exit(0)
if 'plan' in args:
 print(json.dumps({'plan_id':'anonymous-plan','total':2,'eligible':2,'blocked':[],'bytes':12}));sys.exit(0)
if 'status' in args:
 complete=args[-1]=='verified-fixture'
 print(json.dumps({'status':{'complete':complete,'total':1,'verified':1 if complete else 0}}));sys.exit(0)
print(json.dumps({'job_id':'anonymous-job','complete':True,'total':2,'verified':2,'blocked':0}));sys.exit(0)
""")
        fake.chmod(0o700)
        root = directory / "originals"
        root.mkdir()
        state = directory / "state"
        state.mkdir()
        run_id = str(uuid.uuid4())
        command = [sys.executable, str(SCRIPT), "--cli", str(fake), "--state-dir", str(state), "--root", str(root), "--album", "Fixture", "--expect-files", "2", "--run-id", run_id]
        return command, state / "runs" / run_id, calls

    def test_explicit_attempt_is_planned_persisted_and_cannot_change_for_a_run(self):
        with tempfile.TemporaryDirectory() as temporary:
            command, run_directory, calls = self.make_recording_cli(Path(temporary))
            command += ["--attempt-upload-limit", "6442450944", "--require-verified-job", "verified-fixture"]
            result = subprocess.run(command, capture_output=True, timeout=20)
            self.assertEqual(result.returncode, 0, result.stderr)
            status = json.loads((run_directory / "status.json").read_text())
            self.assertEqual(status["configuration"]["attempt_upload_limit"], 6442450944)
            self.assertEqual(status["configuration"]["require_verified_job"], "verified-fixture")
            recorded = [json.loads(line) for line in calls.read_text().splitlines()]
            plans = [args for args in recorded if "plan" in args]
            self.assertEqual(len(plans), 1)
            self.assertEqual(plans[0][-2:], ["--attempt-upload-limit", "6442450944"])
            self.assertTrue(all("--attempt-upload-limit" not in args for args in recorded if "plan" not in args))
            upload_count = sum("upload" in args and "run" in args for args in recorded)
            self.assertEqual(upload_count, 1)
            self.assertEqual(subprocess.run(command, capture_output=True, timeout=10).returncode, 0)
            changed = command.copy()
            changed[changed.index("6442450944")] = "7516192768"
            rejected = subprocess.run(changed, capture_output=True, timeout=10)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn(b"run configuration differs", rejected.stderr)
            recorded = [json.loads(line) for line in calls.read_text().splitlines()]
            self.assertEqual(sum("upload" in args and "run" in args for args in recorded), 1)
            self.assertEqual(sum("plan" in args for args in recorded), 1)

    def test_attempt_still_requires_a_verified_rehearsal(self):
        with tempfile.TemporaryDirectory() as temporary:
            command, run_directory, calls = self.make_recording_cli(Path(temporary))
            command += ["--attempt-upload-limit", "6442450944", "--require-verified-job", "unverified-fixture"]
            result = subprocess.run(command, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 3, result.stderr)
            status = json.loads((run_directory / "status.json").read_text())
            self.assertEqual(status["state"], "awaiting_verified_rehearsal")
            self.assertFalse((run_directory / "result.json").exists())
            recorded = [json.loads(line) for line in calls.read_text().splitlines()]
            self.assertFalse(any("upload" in args and "run" in args for args in recorded))

    def test_attempt_limit_requires_a_positive_integer_before_running_cli(self):
        with tempfile.TemporaryDirectory() as temporary:
            command, _, calls = self.make_recording_cli(Path(temporary))
            for invalid in ["0", "-1", "1.5", "invalid", "18446744073709551616"]:
                result = subprocess.run(command + ["--attempt-upload-limit", invalid], capture_output=True, timeout=10)
                self.assertEqual(result.returncode, 2)
                self.assertIn(b"positive integer", result.stderr)
            self.assertFalse(calls.exists())

    def test_lock_is_exclusive_and_stays_held_by_live_child(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            lock = launcher.runner_lock(directory)
            child = subprocess.Popen([sys.executable, "-c", "import sys;sys.stdin.read()"], stdin=subprocess.PIPE, pass_fds=(lock.fileno(),))
            try:
                lock.close()
                with self.assertRaises(BlockingIOError):
                    launcher.runner_lock(directory)
            finally:
                child.communicate(b"done", timeout=10)
            released = launcher.runner_lock(directory)
            released.close()

    def test_partial_and_planning_failure_propagate_exit_codes(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            fake = directory / "fake-yike"
            fake.write_text(f"#!{sys.executable}\n" + """
import sys,json
args=sys.argv
if 'doctor' in args: print('{}');sys.exit(0)
if 'plan' in args:
 if 'Fail' in args: sys.exit(2)
 print(json.dumps({'plan_id':'anonymous','total':2,'eligible':1,'blocked':[{}],'bytes':12}));sys.exit(0)
print(json.dumps({'job_id':'anonymous-job','complete':False,'total':2,'verified':1,'blocked':1}));sys.exit(3)
""")
            fake.chmod(0o700)
            root = directory / "originals"
            root.mkdir()
            state = directory / "state"
            state.mkdir()
            run_id = str(uuid.uuid4())
            command = [sys.executable, str(SCRIPT), "--cli", str(fake), "--state-dir", str(state), "--root", str(root), "--album", "Fixture", "--expect-files", "2", "--run-id", run_id]
            first = subprocess.run(command, capture_output=True, timeout=20)
            self.assertEqual(first.returncode, 3, first.stderr)
            status = json.loads((state / "runs" / run_id / "status.json").read_text())
            self.assertEqual(status["state"], "partial")
            expected_configuration = {"root": str(root.resolve()), "album": "Fixture", "expected_files": 2, "require_verified_job": None}
            self.assertEqual(json.dumps(status["configuration"]), json.dumps(expected_configuration))
            self.assertEqual(subprocess.run(command, capture_output=True, timeout=10).returncode, 3)
            changed = command.copy()
            changed[changed.index("Fixture")] = "Different"
            self.assertNotEqual(subprocess.run(changed, capture_output=True, timeout=10).returncode, 0)
            fail = command.copy()
            fail[fail.index("Fixture")] = "Fail"
            fail[-1] = str(uuid.uuid4())
            self.assertEqual(subprocess.run(fail, capture_output=True, timeout=10).returncode, 2)
            gated = command.copy()
            gated[-1] = str(uuid.uuid4())
            gated += ["--require-verified-job", "unverified-fixture"]
            self.assertEqual(subprocess.run(gated, capture_output=True, timeout=10).returncode, 3)
            run_directory = state / "runs" / gated[-3]
            self.assertEqual(json.loads((run_directory / "status.json").read_text())["state"], "awaiting_verified_rehearsal")
            self.assertFalse((run_directory / "result.json").exists())


if __name__ == "__main__":
    unittest.main()
