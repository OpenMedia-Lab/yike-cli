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
