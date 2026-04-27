#!/usr/bin/env python3
"""Structured webhook and runtime-policy staging scenarios."""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any


DOCKER_COMPOSE_FILE = "/opt/cmdock/docker-compose.yml"
REMOTE_CMDOCK_ADMIN = "/usr/local/bin/cmdock-admin"


@dataclass
class Config:
    server_url: str
    ssh_host: str
    admin_token: str
    token_a: str
    token_b: str
    user_a_id: str
    user_b_id: str
    sync_enabled: bool
    standalone_cli_ready: bool
    receiver_url: str
    run_webhooks: bool
    run_runtime_policy: bool


def emit(status: str, message: str) -> None:
    print(f"{status}\t{message}")


def parse_args() -> Config:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-url", required=True)
    parser.add_argument("--ssh-host", required=True)
    parser.add_argument("--admin-token", default="")
    parser.add_argument("--token-a", required=True)
    parser.add_argument("--token-b", required=True)
    parser.add_argument("--user-a-id", required=True)
    parser.add_argument("--user-b-id", required=True)
    parser.add_argument("--receiver-url", default="")
    parser.add_argument("--sync-enabled", action="store_true")
    parser.add_argument("--standalone-cli-ready", action="store_true")
    parser.add_argument("--run-webhooks", action="store_true")
    parser.add_argument("--run-runtime-policy", action="store_true")
    args = parser.parse_args()
    return Config(
        server_url=args.server_url.rstrip("/"),
        ssh_host=args.ssh_host,
        admin_token=args.admin_token,
        token_a=args.token_a,
        token_b=args.token_b,
        user_a_id=args.user_a_id,
        user_b_id=args.user_b_id,
        receiver_url=args.receiver_url.rstrip("/"),
        sync_enabled=args.sync_enabled,
        standalone_cli_ready=args.standalone_cli_ready,
        run_webhooks=args.run_webhooks,
        run_runtime_policy=args.run_runtime_policy,
    )


def http_request(
    method: str,
    url: str,
    *,
    token: str | None = None,
    body: dict[str, Any] | None = None,
    timeout: int = 15,
) -> tuple[int, str]:
    data = None if body is None else json.dumps(body).encode()
    headers = {"User-Agent": "cmdock-staging-scenarios"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if body is not None:
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.getcode(), response.read().decode()
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode()


def server_request(config: Config, method: str, path: str, *, token: str | None = None, body: dict[str, Any] | None = None) -> tuple[int, str]:
    return http_request(method, f"{config.server_url}{path}", token=token, body=body)


def server_json(config: Config, method: str, path: str, *, token: str | None = None, body: dict[str, Any] | None = None) -> tuple[int, Any]:
    status, raw = server_request(config, method, path, token=token, body=body)
    if not raw:
        return status, None
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, None


def receiver_json(config: Config, method: str, path: str) -> tuple[int, Any]:
    if not config.receiver_url:
        return 0, None
    status, raw = http_request(method, f"{config.receiver_url}{path}")
    if not raw:
        return status, None
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, None


def receiver_clear(config: Config) -> bool:
    status, _ = receiver_json(config, "DELETE", "/events")
    return status in {200, 204}


def receiver_events(config: Config) -> list[dict[str, Any]]:
    status, payload = receiver_json(config, "GET", "/events")
    if status != 200 or not isinstance(payload, dict):
        return []
    events = payload.get("events")
    return events if isinstance(events, list) else []


def wait_for(seconds: int, predicate) -> bool:
    deadline = time.time() + seconds
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(1)
    return False


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


def admin_cli(config: Config, *args: str) -> subprocess.CompletedProcess[str]:
    remote = f"{shlex.quote(REMOTE_CMDOCK_ADMIN)} --server {shlex.quote(config.server_url)} --token {shlex.quote(config.admin_token)}"
    for arg in args:
        remote += f" {shlex.quote(arg)}"
    return ssh_run(config, remote)


def server_admin(config: Config, *args: str) -> subprocess.CompletedProcess[str]:
    remote = (
        f"sudo docker compose -f {shlex.quote(DOCKER_COMPOSE_FILE)} exec -T server "
        f"cmdock-server --config /app/config.toml"
    )
    for arg in args:
        remote += f" {shlex.quote(arg)}"
    return ssh_run(config, remote)


def record(ok: bool, name: str) -> None:
    emit("PASS" if ok else "FAIL", name)


def skip(name: str) -> None:
    emit("SKIP", name)


def task_uuid_from_create(payload: Any) -> str:
    if not isinstance(payload, dict):
        return ""
    output = payload.get("output")
    if not isinstance(output, str):
        return ""
    match = re.search(r"[0-9a-f-]{36}", output)
    return match.group(0) if match else ""


def webhook_delivery_succeeds(config: Config) -> None:
    if not config.receiver_url:
        emit("FAIL", "Webhook receiver behind Caddy ready")
        return

    emit("PASS", "Webhook receiver behind Caddy ready")
    suffix = str(int(time.time()))
    create_body = {
        "url": f"{config.receiver_url}/hook",
        "secret": "abcdefghijklmnopqrstuvwxyz0123456789",
        "events": ["task.created", "task.modified"],
        "modifiedFields": ["priority"],
        "name": "staging-e2e",
    }
    status, payload = server_json(config, "POST", "/api/webhooks", token=config.token_a, body=create_body)
    webhook_id = payload.get("id") if isinstance(payload, dict) else ""
    record(status == 201 and bool(webhook_id), "POST /api/webhooks")
    if not webhook_id:
        return

    receiver_clear(config)
    status, payload = server_json(
        config,
        "POST",
        "/api/tasks",
        token=config.token_a,
        body={"raw": f"+e2e_staging webhook-create-{suffix}"},
    )
    if not (status == 200 and isinstance(payload, dict) and payload.get("success") is True):
        emit("FAIL", "Webhook fixture: create task for HTTPS delivery")
        return

    def has_signed_created() -> bool:
        return any(
            event.get("body", {}).get("event") == "task.created"
            and str(event.get("headers", {}).get("x-webhook-signature-256", "")).startswith("sha256=")
            for event in receiver_events(config)
        )

    record(wait_for(10, has_signed_created), "Webhook delivery succeeds via HTTPS ingress")

    task_uuid = task_uuid_from_create(payload)
    if not task_uuid:
        emit("FAIL", "Webhook fixture: capture task UUID for modify test")
        return

    receiver_clear(config)
    server_request(
        config,
        "POST",
        f"/api/tasks/{task_uuid}/modify",
        token=config.token_a,
        body={"description": "webhook description-only change"},
    )
    time.sleep(2)
    record(len(receiver_events(config)) == 0, "Webhook modifiedFields filter suppresses non-matching change")

    receiver_clear(config)
    server_request(
        config,
        "POST",
        f"/api/tasks/{task_uuid}/modify",
        token=config.token_a,
        body={"priority": "H"},
    )

    def has_priority_change() -> bool:
        return any(
            event.get("body", {}).get("event") == "task.modified"
            and "priority" in (event.get("body", {}).get("changed_fields") or [])
            for event in receiver_events(config)
        )

    record(wait_for(10, has_priority_change), "Webhook modifiedFields filter emits matching change")

    receiver_clear(config)
    status, payload = server_json(config, "POST", f"/api/webhooks/{webhook_id}/test", token=config.token_a, body={})

    def has_test_event() -> bool:
        return any(event.get("body", {}).get("event") == "webhook.test" for event in receiver_events(config))

    record(
        status == 200
        and isinstance(payload, dict)
        and payload.get("delivery", {}).get("event") == "webhook.test"
        and payload.get("delivery", {}).get("status") == "delivered"
        and wait_for(10, has_test_event),
        "POST /api/webhooks/{id}/test",
    )


def runtime_policy_scenarios(config: Config) -> None:
    if not config.admin_token:
        for name in [
            "GET /admin/user/{id}/runtime-policy starts unmanaged",
            "PUT /admin/user/{id}/runtime-policy block/forbid",
            "Blocked bearer runtime access -> 403",
            "Blocked self-service device provisioning -> 403",
            "Blocked operator device provisioning -> 403",
            "Blocked admin CLI device provisioning",
            "PUT /admin/user/{id}/runtime-policy allow/forbid",
            "Reactivated bearer runtime access -> 200",
            "Allowed self-service device provisioning -> 201",
            "Allowed operator device provisioning -> 201",
            "PUT /admin/user/{id}/runtime-policy allow/forbid for delete gate",
            "Delete forbidden by applied runtime policy",
            "PUT /admin/user/{id}/runtime-policy allow/allow",
        ]:
            skip(name)
        return

    status, payload = server_json(config, "GET", f"/admin/user/{config.user_a_id}/runtime-policy", token=config.admin_token)
    record(
        status == 200
        and isinstance(payload, dict)
        and payload.get("enforcementState") == "unmanaged"
        and payload.get("desiredVersion") is None
        and payload.get("appliedVersion") is None,
        "GET /admin/user/{id}/runtime-policy starts unmanaged",
    )

    status, payload = server_json(
        config,
        "PUT",
        f"/admin/user/{config.user_a_id}/runtime-policy",
        token=config.admin_token,
        body={"policyVersion": "block-v1", "policy": {"runtimeAccess": "block", "deleteAction": "forbid"}},
    )
    record(
        status == 200
        and isinstance(payload, dict)
        and payload.get("desiredVersion") == "block-v1"
        and payload.get("appliedVersion") == "block-v1"
        and payload.get("enforcementState") == "current"
        and payload.get("desiredPolicy", {}).get("runtimeAccess") == "block"
        and payload.get("desiredPolicy", {}).get("deleteAction") == "forbid",
        "PUT /admin/user/{id}/runtime-policy block/forbid",
    )

    status, _ = server_request(config, "GET", "/api/tasks", token=config.token_a)
    record(status == 403, "Blocked bearer runtime access -> 403")

    if config.sync_enabled:
        status, _ = server_request(
            config,
            "POST",
            "/api/devices",
            token=config.token_a,
            body={"name": "Blocked Self Device"},
        )
        record(status == 403, "Blocked self-service device provisioning -> 403")

        status, _ = server_request(
            config,
            "POST",
            f"/admin/user/{config.user_a_id}/devices",
            token=config.admin_token,
            body={"name": "Blocked Operator Device"},
        )
        record(status == 403, "Blocked operator device provisioning -> 403")

        completed = server_admin(config, "admin", "device", "create", config.user_a_id, "--name", "Blocked CLI Device")
        text = completed.stdout + completed.stderr
        record(completed.returncode != 0 and "Runtime access blocked by policy" in text, "Blocked admin CLI device provisioning")
    else:
        skip("Blocked self-service device provisioning -> 403 (sync not enabled)")
        skip("Blocked operator device provisioning -> 403 (sync not enabled)")
        skip("Blocked admin CLI device provisioning (sync not enabled)")

    status, payload = server_json(
        config,
        "PUT",
        f"/admin/user/{config.user_a_id}/runtime-policy",
        token=config.admin_token,
        body={"policyVersion": "allow-v2", "policy": {"runtimeAccess": "allow", "deleteAction": "forbid"}},
    )
    record(
        status == 200
        and isinstance(payload, dict)
        and payload.get("desiredVersion") == "allow-v2"
        and payload.get("appliedVersion") == "allow-v2"
        and payload.get("enforcementState") == "current"
        and payload.get("desiredPolicy", {}).get("runtimeAccess") == "allow"
        and payload.get("desiredPolicy", {}).get("deleteAction") == "forbid",
        "PUT /admin/user/{id}/runtime-policy allow/forbid",
    )

    status, _ = server_request(config, "GET", "/api/tasks", token=config.token_a)
    record(status == 200, "Reactivated bearer runtime access -> 200")

    if config.sync_enabled:
        status, _ = server_request(
            config,
            "POST",
            "/api/devices",
            token=config.token_a,
            body={"name": "Allowed Self Device"},
        )
        record(status == 201, "Allowed self-service device provisioning -> 201")

        status, _ = server_request(
            config,
            "POST",
            f"/admin/user/{config.user_a_id}/devices",
            token=config.admin_token,
            body={"name": "Allowed Operator Device"},
        )
        record(status == 201, "Allowed operator device provisioning -> 201")
    else:
        skip("Allowed self-service device provisioning -> 201 (sync not enabled)")
        skip("Allowed operator device provisioning -> 201 (sync not enabled)")

    status, payload = server_json(
        config,
        "PUT",
        f"/admin/user/{config.user_b_id}/runtime-policy",
        token=config.admin_token,
        body={"policyVersion": "delete-forbid-v1", "policy": {"runtimeAccess": "allow", "deleteAction": "forbid"}},
    )
    record(
        status == 200
        and isinstance(payload, dict)
        and payload.get("desiredVersion") == "delete-forbid-v1"
        and payload.get("appliedVersion") == "delete-forbid-v1"
        and payload.get("enforcementState") == "current"
        and payload.get("desiredPolicy", {}).get("deleteAction") == "forbid",
        "PUT /admin/user/{id}/runtime-policy allow/forbid for delete gate",
    )

    if config.standalone_cli_ready:
        completed = admin_cli(config, "user", "delete", "e2e-user-b", "--yes")
    else:
        completed = server_admin(config, "admin", "user", "delete", config.user_b_id, "--yes")
    text = completed.stdout + completed.stderr
    record(
        completed.returncode != 0 and "applied runtime policy does not allow deletion" in text,
        "Delete forbidden by applied runtime policy",
    )

    status, payload = server_json(
        config,
        "PUT",
        f"/admin/user/{config.user_b_id}/runtime-policy",
        token=config.admin_token,
        body={"policyVersion": "delete-allow-v2", "policy": {"runtimeAccess": "allow", "deleteAction": "allow"}},
    )
    record(
        status == 200
        and isinstance(payload, dict)
        and payload.get("desiredVersion") == "delete-allow-v2"
        and payload.get("appliedVersion") == "delete-allow-v2"
        and payload.get("enforcementState") == "current"
        and payload.get("desiredPolicy", {}).get("deleteAction") == "allow",
        "PUT /admin/user/{id}/runtime-policy allow/allow",
    )


def main() -> int:
    config = parse_args()
    if config.run_webhooks:
        webhook_delivery_succeeds(config)
    if config.run_runtime_policy:
        runtime_policy_scenarios(config)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
