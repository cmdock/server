#!/usr/bin/env python3
"""Structured admin-surface staging scenarios."""

from __future__ import annotations

import argparse
import json
import shlex
import subprocess
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any


DOCKER_COMPOSE_FILE = "/opt/cmdock/docker-compose.yml"


@dataclass
class Config:
    server_url: str
    ssh_host: str
    admin_token: str
    token_a: str
    user_a_id: str
    user_b_id: str
    extra_token_hash: str
    run_admin_http: bool
    run_token_revoke: bool
    run_sync_delete: bool


def emit(status: str, message: str) -> None:
    print(f"{status}\t{message}")


def record(ok: bool, success_name: str, failure_name: str | None = None) -> None:
    emit("PASS" if ok else "FAIL", success_name if ok else (failure_name or success_name))


def skip(name: str) -> None:
    emit("SKIP", name)


def parse_args() -> Config:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-url", required=True)
    parser.add_argument("--ssh-host", required=True)
    parser.add_argument("--admin-token", default="")
    parser.add_argument("--token-a", required=True)
    parser.add_argument("--user-a-id", required=True)
    parser.add_argument("--user-b-id", default="")
    parser.add_argument("--extra-token-hash", default="")
    parser.add_argument("--run-admin-http", action="store_true")
    parser.add_argument("--run-token-revoke", action="store_true")
    parser.add_argument("--run-sync-delete", action="store_true")
    args = parser.parse_args()
    return Config(
        server_url=args.server_url.rstrip("/"),
        ssh_host=args.ssh_host,
        admin_token=args.admin_token,
        token_a=args.token_a,
        user_a_id=args.user_a_id,
        user_b_id=args.user_b_id,
        extra_token_hash=args.extra_token_hash,
        run_admin_http=args.run_admin_http,
        run_token_revoke=args.run_token_revoke,
        run_sync_delete=args.run_sync_delete,
    )


def http_request(
    method: str,
    url: str,
    *,
    token: str | None = None,
    timeout: int = 15,
) -> tuple[int, str]:
    headers = {"User-Agent": "cmdock-staging-admin-runtime"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.getcode(), response.read().decode()
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode()
    except urllib.error.URLError:
        return 0, ""


def server_json(config: Config, method: str, path: str, *, token: str | None = None) -> tuple[int, Any]:
    status, raw = http_request(method, f"{config.server_url}{path}", token=token)
    if not raw:
        return status, None
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, None


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


def server_admin(config: Config, *args: str) -> subprocess.CompletedProcess[str]:
    remote = (
        f"sudo docker compose -f {shlex.quote(DOCKER_COMPOSE_FILE)} exec -T server "
        f"cmdock-server --config /app/config.toml"
    )
    for arg in args:
        remote += f" {shlex.quote(arg)}"
    return ssh_run(config, remote)


def run_admin_http(config: Config) -> None:
    if not config.admin_token:
        skip("ADMIN HTTP: set STAGING_ADMIN_TOKEN or CMDOCK_ADMIN_TOKEN to exercise /admin/*")
        skip("GET /admin/status")
        skip("GET /admin/user/{id}/stats")
        skip("GET /admin/user/{id}/stats?integrity=quick")
        skip("POST /admin/user/{id}/evict")
        skip("POST /admin/user/{id}/checkpoint")
        skip("POST /admin/user/{id}/offline (quarantine)")
        skip("Quarantined user gets 503")
        skip("POST /admin/user/{id}/online (unquarantine)")
        skip("Unquarantined user gets 200")
        return

    status, payload = server_json(config, "GET", "/admin/status", token=config.admin_token)
    record(
        status == 200 and isinstance(payload, dict) and payload.get("uptime_seconds") is not None,
        "GET /admin/status",
    )

    status, payload = server_json(
        config,
        "GET",
        f"/admin/user/{config.user_a_id}/stats",
        token=config.admin_token,
    )
    record(
        status == 200 and isinstance(payload, dict) and payload.get("user_id") == config.user_a_id,
        "GET /admin/user/{id}/stats",
    )

    status, payload = server_json(
        config,
        "GET",
        f"/admin/user/{config.user_a_id}/stats?integrity=quick",
        token=config.admin_token,
    )
    record(
        status == 200 and isinstance(payload, dict) and payload.get("user_id") == config.user_a_id,
        "GET /admin/user/{id}/stats?integrity=quick",
    )

    status, _ = http_request(
        "POST",
        f"{config.server_url}/admin/user/{config.user_a_id}/evict",
        token=config.admin_token,
    )
    record(status == 200, "POST /admin/user/{id}/evict", f"POST /admin/user/{{id}}/evict ({status})")

    status, _ = http_request(
        "POST",
        f"{config.server_url}/admin/user/{config.user_a_id}/checkpoint",
        token=config.admin_token,
    )
    record(
        status == 200,
        "POST /admin/user/{id}/checkpoint",
        f"POST /admin/user/{{id}}/checkpoint ({status})",
    )

    status, _ = http_request(
        "POST",
        f"{config.server_url}/admin/user/{config.user_a_id}/offline",
        token=config.admin_token,
    )
    record(
        status == 200,
        "POST /admin/user/{id}/offline (quarantine)",
        f"POST /admin/user/{{id}}/offline ({status})",
    )

    status, _ = http_request("GET", f"{config.server_url}/api/tasks", token=config.token_a)
    record(status == 503, "Quarantined user gets 503", f"Quarantined user gets {status}")

    status, _ = http_request(
        "POST",
        f"{config.server_url}/admin/user/{config.user_a_id}/online",
        token=config.admin_token,
    )
    record(
        status == 200,
        "POST /admin/user/{id}/online (unquarantine)",
        f"POST /admin/user/{{id}}/online ({status})",
    )

    status, _ = http_request("GET", f"{config.server_url}/api/tasks", token=config.token_a)
    record(status == 200, "Unquarantined user gets 200", f"Unquarantined user gets {status}")


def run_token_revoke(config: Config) -> None:
    if not config.extra_token_hash:
        skip("admin token revoke (no hash captured)")
        return
    completed = server_admin(config, "admin", "token", "revoke", config.extra_token_hash, "--yes")
    output = f"{completed.stdout}\n{completed.stderr}".lower()
    record(
        "revoked" in output or "deleted" in output,
        "admin token revoke",
        f"admin token revoke (hash={config.extra_token_hash}, output={output.strip()})",
    )


def run_sync_delete(config: Config) -> None:
    if not config.user_b_id:
        skip("admin sync delete (sync not enabled or no client_id)")
        return
    completed = server_admin(config, "admin", "sync", "delete", config.user_b_id)
    delete_output = f"{completed.stdout}\n{completed.stderr}".lower()
    record(
        "deleted" in delete_output or "removed" in delete_output or "no replica" in delete_output,
        "admin sync delete",
        f"admin sync delete (output: {delete_output.strip()})",
    )

    completed = server_admin(config, "admin", "sync", "show", config.user_b_id)
    show_output = f"{completed.stdout}\n{completed.stderr}".lower()
    record(
        "no replica" in show_output
        or "not found" in show_output
        or "no canonical sync identity" in show_output,
        "Sync identity removed",
        f"Sync identity not removed (output: {show_output.strip()})",
    )


def main() -> int:
    config = parse_args()
    if config.run_admin_http:
        run_admin_http(config)
    if config.run_token_revoke:
        run_token_revoke(config)
    if config.run_sync_delete:
        run_sync_delete(config)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
