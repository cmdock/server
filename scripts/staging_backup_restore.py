#!/usr/bin/env python3
"""Structured backup/restore staging scenario runner.

This owns the JSON-heavy backup and restore verification flow and emits
tab-separated result records for the legacy shell harness to count.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone


DOCKER_COMPOSE_FILE = "/opt/cmdock/docker-compose.yml"
REMOTE_CMDOCK_ADMIN = "/usr/local/bin/cmdock-admin"


@dataclass
class Config:
    server_url: str
    ssh_host: str
    admin_token: str
    token_a: str
    token_b: str
    user_b_id: str
    sync_enabled: bool
    tw_mode: str
    tw_host: str
    tw_dir_a: str
    tw_dir_b: str
    timings_json: str


def emit(status: str, message: str) -> None:
    print(f"{status}\t{message}")


def parse_args() -> Config:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-url", required=True)
    parser.add_argument("--ssh-host", required=True)
    parser.add_argument("--admin-token", required=True)
    parser.add_argument("--token-a", required=True)
    parser.add_argument("--token-b", required=True)
    parser.add_argument("--user-b-id", required=True)
    parser.add_argument("--sync-enabled", action="store_true")
    parser.add_argument("--tw-mode", choices=["ssh", "local"], required=True)
    parser.add_argument("--tw-host", default="")
    parser.add_argument("--tw-dir-a", default="")
    parser.add_argument("--tw-dir-b", default="")
    parser.add_argument("--timings-json", default="")
    args = parser.parse_args()
    return Config(
        server_url=args.server_url.rstrip("/"),
        ssh_host=args.ssh_host,
        admin_token=args.admin_token,
        token_a=args.token_a,
        token_b=args.token_b,
        user_b_id=args.user_b_id,
        sync_enabled=args.sync_enabled,
        tw_mode=args.tw_mode,
        tw_host=args.tw_host or args.ssh_host,
        tw_dir_a=args.tw_dir_a,
        tw_dir_b=args.tw_dir_b,
        timings_json=args.timings_json,
    )


def ssh_run(config: Config, remote_command: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            "ssh",
            "-o",
            "ControlMaster=no",
            "-o",
            "ConnectTimeout=10",
            config.ssh_host,
            remote_command,
        ],
        check=False,
        capture_output=True,
        text=True,
    )


def tw_run(config: Config, directory: str, *args: str) -> subprocess.CompletedProcess[str]:
    if config.tw_mode == "ssh":
        cmd = f"TASKRC={shlex.quote(directory + '/taskrc')} TASKDATA={shlex.quote(directory + '/data')} task"
        for arg in args:
            cmd += f" {shlex.quote(arg)}"
        return subprocess.run(
            [
                "ssh",
                "-o",
                "ControlMaster=no",
                "-o",
                "ConnectTimeout=10",
                config.tw_host,
                cmd,
            ],
            check=False,
            capture_output=True,
            text=True,
        )

    env = dict(**os.environ, TASKRC=f"{directory}/taskrc", TASKDATA=f"{directory}/data")
    return subprocess.run(
        ["task", *args],
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )


def rest_request(
    config: Config,
    method: str,
    path: str,
    token: str,
    body: dict | None = None,
) -> tuple[int, str]:
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(
        f"{config.server_url}{path}",
        data=data,
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "User-Agent": "cmdock-staging-backup-restore",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            return response.getcode(), response.read().decode()
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode()


def rest_json(
    config: Config,
    method: str,
    path: str,
    token: str,
    body: dict | None = None,
) -> tuple[int, dict | list | None]:
    status, raw = rest_request(config, method, path, token, body)
    if not raw:
        return status, None
    return status, json.loads(raw)


def admin_cli_json(config: Config, *args: str) -> tuple[int, dict | list | None, str]:
    remote = f"{shlex.quote(REMOTE_CMDOCK_ADMIN)} --server {shlex.quote(config.server_url)} --token {shlex.quote(config.admin_token)} --json"
    for arg in args:
        remote += f" {shlex.quote(arg)}"
    completed = ssh_run(config, remote)
    output = "\n".join(
        line for line in completed.stdout.splitlines() if not line.startswith('{"timestamp"')
    ).strip()
    if not output:
        output = "\n".join(
            line for line in completed.stderr.splitlines() if not line.startswith('{"timestamp"')
        ).strip()
    parsed = None
    if output:
        try:
            parsed = json.loads(output)
        except json.JSONDecodeError:
            parsed = None
    return completed.returncode, parsed, output


def server_admin_sync_delete(config: Config, user_id: str) -> subprocess.CompletedProcess[str]:
    remote = (
        f"sudo docker compose -f {shlex.quote(DOCKER_COMPOSE_FILE)} exec -T server "
        f"cmdock-server --config /app/config.toml admin sync delete {shlex.quote(user_id)}"
    )
    return ssh_run(config, remote)


def task_exists(config: Config, token: str, needle: str) -> bool:
    status, body = rest_json(config, "GET", "/api/tasks", token)
    if status != 200 or not isinstance(body, list):
        return False
    return any(needle in (task.get("description") or "") for task in body)


def context_exists(config: Config, token: str, context_id: str) -> bool:
    status, body = rest_json(config, "GET", "/api/contexts", token)
    if status != 200 or not isinstance(body, list):
        return False
    return any(entry.get("id") == context_id for entry in body)


def admin_user_has_recent_sync(config: Config, username: str) -> bool:
    status, body = rest_json(config, "GET", "/admin/users", config.admin_token)
    if status != 200 or not isinstance(body, list):
        return False
    for user in body:
        if user.get("username") != username:
            continue
        last_sync = user.get("lastSyncAt")
        if not last_sync:
            return False
        if last_sync.endswith("Z"):
            last_sync = last_sync[:-1] + "+00:00"
        observed = datetime.fromisoformat(last_sync)
        return (datetime.now(timezone.utc) - observed.astimezone(timezone.utc)).total_seconds() <= 86400
    return False


def doctor_passes(config: Config) -> bool:
    code, payload, _ = admin_cli_json(config, "doctor")
    if code != 0 or not isinstance(payload, dict):
        return False
    checks = payload.get("checks")
    if not isinstance(checks, list):
        return False
    return not any(check.get("status") == "fail" for check in checks)


def wait_for(description: str, seconds: int, predicate) -> bool:
    deadline = time.time() + seconds
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(1)
    return False


def record(status: bool, success_name: str, failure_name: str | None = None) -> None:
    emit("PASS" if status else "FAIL", success_name if status else (failure_name or success_name))


def timed_admin_cli_json(config: Config, *args: str) -> tuple[int, dict | list | None, str, float]:
    started = time.monotonic()
    code, payload, output = admin_cli_json(config, *args)
    return code, payload, output, time.monotonic() - started


def main() -> int:
    config = parse_args()
    timings: dict[str, object] = {}
    suffix = str(int(time.time()))
    restore_pre_task = f"restore-pre-task-{suffix}"
    restore_post_task = f"restore-post-task-{suffix}"
    restore_pre_context = f"restore-pre-context-{suffix}"
    restore_post_context = f"restore-post-context-{suffix}"
    dr_pre_task = f"dr-pre-task-{suffix}"
    dr_post_task = f"dr-post-task-{suffix}"
    dr_sync_task = f"dr-sync-task-{suffix}"

    try:
        status, payload = rest_json(
            config,
            "POST",
            "/api/tasks",
            config.token_a,
            {"raw": f"+e2e_staging {restore_pre_task}"},
        )
        record(status == 200 and isinstance(payload, dict) and payload.get("success") is True, "Backup fixture: create pre-backup task")

        status, _ = rest_request(
            config,
            "PUT",
            f"/api/contexts/{restore_pre_context}",
            config.token_a,
            {
                "label": "Restore Fixture",
                "projectPrefixes": ["RESTORE"],
                "color": "#2266AA",
                "icon": "archive",
            },
        )
        record(status == 200, "Backup fixture: create pre-backup context")

        code, backup_json, backup_out, backup_seconds = timed_admin_cli_json(config, "backup")
        timings["full_backup_seconds"] = round(backup_seconds, 3)
        backup_timestamp = backup_json.get("timestamp") if isinstance(backup_json, dict) else ""
        record(bool(backup_timestamp) and code == 0, "cmdock-admin backup", f"cmdock-admin backup ({backup_out[:180]})")

        if not backup_timestamp:
            return 0

        code, backup_list, _ = admin_cli_json(config, "backup", "list")
        listed = isinstance(backup_list, list) and any(
            entry.get("timestamp") == backup_timestamp and entry.get("backupType") == "full"
            for entry in backup_list
        )
        record(code == 0 and listed, "cmdock-admin backup list includes created snapshot")

        status, payload = rest_json(
            config,
            "POST",
            "/api/tasks",
            config.token_a,
            {"raw": f"+e2e_staging {restore_post_task}"},
        )
        record(status == 200 and isinstance(payload, dict) and payload.get("success") is True, "Backup fixture: create post-backup task")

        status, _ = rest_request(
            config,
            "PUT",
            f"/api/contexts/{restore_post_context}",
            config.token_a,
            {
                "label": "Restore Post Fixture",
                "projectPrefixes": ["POSTRESTORE"],
                "color": "#884422",
                "icon": "history",
            },
        )
        record(status == 200, "Backup fixture: create post-backup context")

        code, restore_json, restore_out, restore_seconds = timed_admin_cli_json(
            config, "backup", "restore", backup_timestamp, "--yes"
        )
        timings["full_restore_seconds"] = round(restore_seconds, 3)
        pre_restore_snapshot = restore_json.get("preRestoreSnapshot") if isinstance(restore_json, dict) else ""
        restored_ok = (
            code == 0
            and isinstance(restore_json, dict)
            and restore_json.get("restoredFrom") == backup_timestamp
        )
        record(restored_ok, "cmdock-admin backup restore", f"cmdock-admin backup restore ({restore_out[:180]})")
        record(bool(pre_restore_snapshot), "Backup restore creates pre-restore snapshot")

        code, backup_list_after, _ = admin_cli_json(config, "backup", "list")
        pre_restore_list_ok = (
            code == 0
            and isinstance(backup_list_after, list)
            and any(
                entry.get("timestamp") == pre_restore_snapshot
                and entry.get("backupType") == "pre_restore"
                for entry in backup_list_after
            )
        )
        record(pre_restore_list_ok, "cmdock-admin backup list includes pre-restore snapshot")
        record(task_exists(config, config.token_a, restore_pre_task), "Backup restore restores pre-backup task")
        record(not task_exists(config, config.token_a, restore_post_task), "Backup restore removes post-backup task")
        record(context_exists(config, config.token_a, restore_pre_context), "Backup restore restores pre-backup context")
        record(not context_exists(config, config.token_a, restore_post_context), "Backup restore removes post-backup context")

        if config.sync_enabled:
            sync_ok = tw_run(config, config.tw_dir_a, "sync").returncode == 0 and tw_run(config, config.tw_dir_b, "sync").returncode == 0
            record(sync_ok, "Backup restore preserves Taskwarrior sync path")
        else:
            emit("SKIP", "Backup restore preserves Taskwarrior sync path (sync not enabled)")

        status, payload = rest_json(
            config,
            "POST",
            "/api/tasks",
            config.token_b,
            {"raw": f"+e2e_staging {dr_pre_task}"},
        )
        record(status == 200 and isinstance(payload, dict) and payload.get("success") is True, "DR fixture: create pre-backup task")

        code, dr_backup_json, dr_backup_out, dr_backup_seconds = timed_admin_cli_json(
            config, "backup", "--include-secrets"
        )
        timings["dr_backup_seconds"] = round(dr_backup_seconds, 3)
        dr_backup_timestamp = dr_backup_json.get("timestamp") if isinstance(dr_backup_json, dict) else ""
        record(bool(dr_backup_timestamp) and code == 0, "cmdock-admin backup --include-secrets", f"cmdock-admin backup --include-secrets ({dr_backup_out[:180]})")

        if not dr_backup_timestamp:
            return 0

        code, dr_list_json, _ = admin_cli_json(config, "backup", "list")
        dr_list_ok = (
            code == 0
            and isinstance(dr_list_json, list)
            and any(
                entry.get("timestamp") == dr_backup_timestamp and entry.get("secretsIncluded") is True
                for entry in dr_list_json
            )
        )
        record(dr_list_ok, "cmdock-admin backup list marks secrets-included snapshot")

        status, payload = rest_json(
            config,
            "POST",
            "/api/tasks",
            config.token_b,
            {"raw": f"+e2e_staging {dr_post_task}"},
        )
        record(status == 200 and isinstance(payload, dict) and payload.get("success") is True, "DR fixture: create post-backup task")

        if config.sync_enabled:
            delete_out = server_admin_sync_delete(config, config.user_b_id)
            delete_ok = any(word in (delete_out.stdout + delete_out.stderr).lower() for word in ["deleted", "removed", "no replica"])
            record(delete_ok, "DR corruption: delete live sync identity")

            def corrupt_stats() -> bool:
                code, body = rest_json(config, "GET", f"/admin/user/{config.user_b_id}/stats", config.admin_token)
                return (
                    code == 200
                    and isinstance(body, dict)
                    and body.get("recovery_assessment", {}).get("status") == "needs_operator_attention"
                    and body.get("recovery_assessment", {}).get("sync_identity_exists") is False
                )

            record(
                wait_for("dr-corruption-marks-user-needs-operator-attention", 5, corrupt_stats),
                "DR corruption marks affected user needs operator attention",
            )
        else:
            emit("SKIP", "DR corruption: delete live sync identity (sync not enabled)")
            emit("SKIP", "DR corruption marks affected user needs operator attention (sync not enabled)")

        code, dr_restore_json, dr_restore_out, dr_restore_seconds = timed_admin_cli_json(
            config, "backup", "restore", dr_backup_timestamp, "--yes"
        )
        timings["dr_restore_seconds"] = round(dr_restore_seconds, 3)
        dr_restore_ok = (
            code == 0
            and isinstance(dr_restore_json, dict)
            and dr_restore_json.get("restoredFrom") == dr_backup_timestamp
            and dr_restore_json.get("secretsRestored") is True
        )
        record(dr_restore_ok, "cmdock-admin backup restore (DR-style snapshot)", f"cmdock-admin backup restore (DR-style snapshot) ({dr_restore_out[:180]})")
        record(task_exists(config, config.token_b, dr_pre_task), "DR-style restore restores pre-backup task")
        record(not task_exists(config, config.token_b, dr_post_task), "DR-style restore removes post-backup task")

        if config.sync_enabled:
            tw_run(config, config.tw_dir_a, "add", f"+e2e_staging {dr_sync_task}")
            sync_ok = tw_run(config, config.tw_dir_a, "sync").returncode == 0 and wait_for(
                "dr-sync-restores-sync-path", 5, lambda: task_exists(config, config.token_b, dr_sync_task)
            )
            record(sync_ok, "DR-style restore preserves Taskwarrior sync path")
            record(
                wait_for(
                    "admin-users-recent-sync-after-dr-restore",
                    5,
                    lambda: admin_user_has_recent_sync(config, "e2e-user-b"),
                ),
                "DR-style restore updates recent admin user sync timestamp",
            )
            doctor_after_dr_restore = doctor_passes(config)
            timings["doctor_after_dr_restore_passed"] = doctor_after_dr_restore
            record(doctor_after_dr_restore, "cmdock-admin doctor after DR-style restore")
        else:
            emit("SKIP", "DR-style restore preserves Taskwarrior sync path (sync not enabled)")
            emit("SKIP", "DR-style restore updates recent admin user sync timestamp (sync not enabled)")
            doctor_after_dr_restore = doctor_passes(config)
            timings["doctor_after_dr_restore_passed"] = doctor_after_dr_restore
            record(doctor_after_dr_restore, "cmdock-admin doctor after DR-style restore")
    except Exception as exc:  # pragma: no cover - defensive scenario wrapper
        emit("FAIL", f"Backup/restore scenario runner crashed ({exc})")
    finally:
        if config.timings_json:
            with open(config.timings_json, "w", encoding="utf-8") as handle:
                json.dump(timings, handle, indent=2, sort_keys=True)
                handle.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
