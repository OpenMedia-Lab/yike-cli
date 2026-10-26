#!/usr/bin/env python3
"""POSIX/WSL durable plan -> verified upload launcher; all data stays on this host.

The Rust CLI owns transfer semantics, account locks, validation and credentials.
This helper merely keeps an unattended planning/upload process and a local status
file alive across SSH disconnects. It never deletes source or cloud objects.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import time
import uuid


def runner_lock(directory):
    """OS releases this lock after a crash; two launchers cannot share a run."""
    lock = (directory / "runner.lock").open("a+b")
    os.chmod(directory / "runner.lock", 0o600)
    import fcntl
    try:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BaseException:
        lock.close()
        raise
    return lock


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def write_status(path, status):
    status["updated_at"] = now()
    temporary = path.with_suffix(".new")
    with temporary.open("w", encoding="utf-8") as stream:
        os.chmod(temporary, 0o600)
        json.dump(status, stream, ensure_ascii=False, indent=2)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def find_job(database, plan_id):
    try:
        connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=2)
        try:
            for (body,) in connection.execute("SELECT body FROM records WHERE kind='jobs' ORDER BY updated DESC"):
                job = json.loads(body)
                if job["plan_id"] == plan_id and job["kind"] == "upload":
                    return job["id"]
        finally:
            connection.close()
    except (sqlite3.Error, OSError, ValueError):
        pass
    return None


def run():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--album", required=True)
    parser.add_argument("--expect-files", type=int, required=True)
    parser.add_argument("--require-verified-job", help="Do not start upload until this rehearsal job is fully SHA-256 verified")
    parser.add_argument("--run-id", default=str(uuid.uuid4()))
    arguments = parser.parse_args()
    if os.name == "nt":
        parser.error("this optional background launcher requires POSIX/WSL; use native yike upload/jobs commands on Windows")
    # UUID restricts the local output directory; user strings never enter a shell.
    run_id = str(uuid.UUID(arguments.run_id))
    command = [str(arguments.cli.resolve()), "--state-dir", str(arguments.state_dir.resolve()), "--json"]
    subprocess.run(command + ["doctor", "--offline"], check=True, stdout=subprocess.DEVNULL)
    directory = arguments.state_dir.resolve() / "runs" / run_id
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(directory, 0o700)
    lock = runner_lock(directory)
    # If the launcher dies while Rust is still transferring, the inherited flock
    # prevents another launcher from truncating output or starting a second child.
    child_options = {"pass_fds": (lock.fileno(),)}
    status_path = directory / "status.json"
    configuration = {"root": str(arguments.root.resolve()), "album": arguments.album, "expected_files": arguments.expect_files, "require_verified_job": arguments.require_verified_job}
    status = json.loads(status_path.read_text()) if status_path.exists() else {"run_id": run_id, "created_at": now(), "configuration": configuration}
    if status.get("configuration") != configuration:
        raise ValueError("run configuration differs; use a new run-id for a different source or album")
    if status.get("state") in ("completed", "partial"):
        return 0 if status["state"] == "completed" else 3
    log_path = directory / "progress.log"
    with log_path.open("ab", buffering=0) as log:
        os.chmod(log_path, 0o600)
        if "plan_id" not in status:
            status["state"] = "planning"
            write_status(status_path, status)
            result_path = directory / "plan.json"
            with result_path.open("wb") as result:
                os.chmod(result_path, 0o600)
                process = subprocess.Popen(command + ["upload", "plan", configuration["root"], "--album", arguments.album, "--expect-files", str(arguments.expect_files)], stdout=result, stderr=log, **child_options)
                status["child_pid"] = process.pid
                write_status(status_path, status)
                code = process.wait()
            if code:
                status.update(state="planning_failed", exit_code=code)
                write_status(status_path, status)
                return code
            plan = json.loads(result_path.read_text())
            status.update(plan_id=plan["plan_id"], total=plan["total"], eligible=plan["eligible"], blocked=len(plan["blocked"]), bytes=plan["bytes"])
        if arguments.require_verified_job:
            gate = subprocess.run(command + ["jobs", "status", arguments.require_verified_job], stdout=subprocess.PIPE, stderr=log)
            try:
                gate_status = json.loads(gate.stdout)["status"] if gate.returncode == 0 else {}
            except (ValueError, KeyError):
                gate_status = {}
            if not (gate_status.get("complete") is True and gate_status.get("total", 0) > 0 and gate_status.get("verified") == gate_status.get("total")):
                status.update(state="awaiting_verified_rehearsal", exit_code=3)
                write_status(status_path, status)
                return 3
        job_id = status.get("job_id") or find_job(arguments.state_dir / "state.sqlite3", status["plan_id"])
        operation = ["jobs", "resume", job_id] if job_id else ["upload", "run", status["plan_id"]]
        status["state"] = "uploading"
        write_status(status_path, status)
        result_path = directory / "result.json"
        with result_path.open("wb") as result:
            os.chmod(result_path, 0o600)
            process = subprocess.Popen(command + operation, stdout=result, stderr=log, **child_options)
            status["child_pid"] = process.pid
            while process.poll() is None:
                job_id = job_id or find_job(arguments.state_dir / "state.sqlite3", status["plan_id"])
                if job_id:
                    status["job_id"] = job_id
                write_status(status_path, status)
                time.sleep(5)
            code = process.returncode
        try:
            result = json.loads(result_path.read_text())
        except (ValueError, OSError):
            result = {}
        if code == 0 and not result.get("complete"):
            code = 2
        status.update(state="completed" if result.get("complete") and code == 0 else "partial" if code == 3 else "upload_failed", exit_code=code, result=result)
        if result.get("job_id"):
            status["job_id"] = result["job_id"]
        write_status(status_path, status)
    lock.close()
    return code


if __name__ == "__main__":
    raise SystemExit(run())
