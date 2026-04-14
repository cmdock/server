#!/usr/bin/env python3
"""Structured product-flow staging scenarios.

This runner owns the JSON-heavy REST, app-config, sync, restart, and latency
checks that were previously inlined in the legacy shell harness.
"""

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


@dataclass
class Config:
    server_url: str
    ssh_host: str
    token_a: str
    token_b: str
    sync_enabled: bool
    tw_mode: str
    tw_host: str
    tw_dir_a: str
    tw_dir_b: str
    run_rest_crud: bool
    run_app_config: bool
    run_sync_e2e: bool
    run_restart_latency: bool


def emit(status: str, message: str) -> None:
    print(f"{status}\t{message}")


def parse_args() -> Config:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-url", required=True)
    parser.add_argument("--ssh-host", required=True)
    parser.add_argument("--token-a", required=True)
    parser.add_argument("--token-b", required=True)
    parser.add_argument("--sync-enabled", action="store_true")
    parser.add_argument("--tw-mode", choices=["ssh", "local"], default="ssh")
    parser.add_argument("--tw-host", default="")
    parser.add_argument("--tw-dir-a", default="")
    parser.add_argument("--tw-dir-b", default="")
    parser.add_argument("--run-rest-crud", action="store_true")
    parser.add_argument("--run-app-config", action="store_true")
    parser.add_argument("--run-sync-e2e", action="store_true")
    parser.add_argument("--run-restart-latency", action="store_true")
    args = parser.parse_args()
    return Config(
        server_url=args.server_url.rstrip("/"),
        ssh_host=args.ssh_host,
        token_a=args.token_a,
        token_b=args.token_b,
        sync_enabled=args.sync_enabled,
        tw_mode=args.tw_mode,
        tw_host=args.tw_host or args.ssh_host,
        tw_dir_a=args.tw_dir_a,
        tw_dir_b=args.tw_dir_b,
        run_rest_crud=args.run_rest_crud,
        run_app_config=args.run_app_config,
        run_sync_e2e=args.run_sync_e2e,
        run_restart_latency=args.run_restart_latency,
    )


def record(ok: bool, success_name: str, failure_name: str | None = None) -> None:
    emit("PASS" if ok else "FAIL", success_name if ok else (failure_name or success_name))


def skip(name: str) -> None:
    emit("SKIP", name)


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


def http_request(
    method: str,
    url: str,
    *,
    token: str | None = None,
    body: dict[str, Any] | None = None,
    timeout: int = 15,
) -> tuple[int, str]:
    data = None if body is None else json.dumps(body).encode()
    headers = {"User-Agent": "cmdock-staging-product-runtime"}
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
    except urllib.error.URLError:
        return 0, ""


def server_request(
    config: Config,
    method: str,
    path: str,
    *,
    token: str | None = None,
    body: dict[str, Any] | None = None,
    timeout: int = 15,
) -> tuple[int, str]:
    return http_request(
        method,
        f"{config.server_url}{path}",
        token=token,
        body=body,
        timeout=timeout,
    )


def server_json(
    config: Config,
    method: str,
    path: str,
    *,
    token: str | None = None,
    body: dict[str, Any] | None = None,
    timeout: int = 15,
) -> tuple[int, Any]:
    status, raw = server_request(
        config, method, path, token=token, body=body, timeout=timeout
    )
    if not raw:
        return status, None
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, None


def wait_for(seconds: int, predicate) -> bool:
    deadline = time.time() + seconds
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(1)
    return False


def task_uuid_from_output(payload: Any) -> str:
    if not isinstance(payload, dict):
        return ""
    output = payload.get("output")
    if not isinstance(output, str):
        return ""
    match = re.search(r"[0-9a-f-]{36}", output)
    return match.group(0) if match else ""


def tw_run(config: Config, directory: str, *args: str) -> subprocess.CompletedProcess[str]:
    if config.tw_mode == "ssh":
        cmd = (
            f"TASKRC={shlex.quote(directory + '/taskrc')} "
            f"TASKDATA={shlex.quote(directory + '/data')} task"
        )
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


def tw_export(config: Config, directory: str) -> list[dict[str, Any]]:
    completed = tw_run(config, directory, "export")
    if completed.returncode != 0:
        return []
    try:
        payload = json.loads(completed.stdout or "[]")
    except json.JSONDecodeError:
        return []
    return payload if isinstance(payload, list) else []


def tw_sync_has_description(config: Config, directory: str, description: str) -> bool:
    tw_run(config, directory, "sync")
    return any(description in str(task.get("description", "")) for task in tw_export(config, directory))


def tw_field_for_uuid(config: Config, directory: str, uuid: str, field: str) -> str:
    for task in tw_export(config, directory):
        if task.get("uuid") == uuid:
            value = task.get(field)
            return "" if value is None else str(value)
    return ""


def tw_uuid_for_description_contains(config: Config, directory: str, needle: str) -> str:
    for task in tw_export(config, directory):
        description = str(task.get("description", ""))
        if needle in description:
            return str(task.get("uuid", ""))
    return ""


def task_count(config: Config, token: str) -> int | None:
    status, payload = server_json(config, "GET", "/api/tasks", token=token)
    if status != 200 or not isinstance(payload, list):
        return None
    return len(payload)


def rest_task_exists(config: Config, token: str, needle: str) -> bool:
    status, payload = server_json(config, "GET", "/api/tasks", token=token)
    if status != 200 or not isinstance(payload, list):
        return False
    return any(needle in str(task.get("description", "")) for task in payload)


def run_rest_crud(config: Config) -> None:
    status, _ = server_request(config, "GET", "/api/tasks", token=config.token_a, timeout=5)
    record(status == 200, "Valid token → 200", f"Valid token → {status}")

    status, _ = server_request(config, "GET", "/api/tasks", token="bad-token-xxx", timeout=5)
    record(status == 401, "Invalid token → 401", f"Invalid token → {status}")

    status, _ = server_request(config, "GET", "/api/tasks", timeout=5)
    record(status == 401, "No token → 401", f"No token → {status}")

    status, payload = server_json(
        config,
        "POST",
        "/api/tasks",
        token=config.token_a,
        body={"raw": "+e2e_staging REST-created task project:E2E priority:H"},
    )
    record(
        status == 200 and isinstance(payload, dict) and payload.get("success") is True,
        "POST /api/tasks creates task",
    )

    task_uuid = task_uuid_from_output(payload)
    status, tasks = server_json(config, "GET", "/api/tasks", token=config.token_a)
    record(
        status == 200
        and isinstance(tasks, list)
        and bool(task_uuid)
        and any(task.get("uuid") == task_uuid for task in tasks),
        "GET /api/tasks returns created task",
        f"GET /api/tasks — created task {task_uuid or '<missing>'} not found",
    )

    if task_uuid:
        status, payload = server_json(
            config,
            "POST",
            f"/api/tasks/{task_uuid}/modify",
            token=config.token_a,
            body={"priority": "L"},
        )
        record(
            status == 200 and isinstance(payload, dict) and payload.get("success") is True,
            "POST /api/tasks/{uuid}/modify",
        )

        status, payload = server_json(
            config, "POST", f"/api/tasks/{task_uuid}/done", token=config.token_a
        )
        record(
            status == 200 and isinstance(payload, dict) and payload.get("success") is True,
            "POST /api/tasks/{uuid}/done",
        )

        status, payload = server_json(
            config, "POST", f"/api/tasks/{task_uuid}/undo", token=config.token_a
        )
        record(
            status == 200 and isinstance(payload, dict) and payload.get("success") is True,
            "POST /api/tasks/{uuid}/undo",
        )
    else:
        emit("FAIL", "POST /api/tasks/{uuid}/modify")
        emit("FAIL", "POST /api/tasks/{uuid}/done")
        emit("FAIL", "POST /api/tasks/{uuid}/undo")

    server_request(
        config,
        "POST",
        "/api/tasks",
        token=config.token_a,
        body={"raw": "+e2e_staging REST task for sync test"},
    )

    status, payload = server_json(
        config,
        "POST",
        "/api/tasks",
        token=config.token_a,
        body={"raw": "+e2e_staging Task to delete"},
    )
    delete_uuid = task_uuid_from_output(payload)
    if delete_uuid:
        status, payload = server_json(
            config, "POST", f"/api/tasks/{delete_uuid}/delete", token=config.token_a
        )
        record(
            status == 200 and isinstance(payload, dict) and payload.get("success") is True,
            "POST /api/tasks/{uuid}/delete",
        )
    else:
        emit("FAIL", "POST /api/tasks/{uuid}/delete (could not create task to delete)")

    status, payload = server_json(config, "GET", "/api/tasks", token=config.token_b)
    record(
        status == 200 and isinstance(payload, list) and len(payload) == 0,
        "User B cannot see User A tasks",
    )


def run_app_config(config: Config) -> None:
    status, payload = server_json(config, "GET", "/api/app-config", token=config.token_a)
    views = payload.get("views") if isinstance(payload, dict) else None
    contexts = payload.get("contexts") if isinstance(payload, dict) else None
    presets = payload.get("presets") if isinstance(payload, dict) else None
    stores = payload.get("stores") if isinstance(payload, dict) else None
    record(status == 200 and isinstance(views, list), "GET /api/app-config")
    record(isinstance(contexts, list), "GET /api/app-config has contexts")
    record(isinstance(presets, list), "GET /api/app-config has presets")
    record(isinstance(stores, list), "GET /api/app-config has stores")
    view_count = len(views) if isinstance(views, list) else 0
    record(
        view_count >= 6,
        f"GET /api/app-config includes default views ({view_count})",
        f"GET /api/app-config missing default views (got {view_count})",
    )

    status, _ = server_request(
        config,
        "PUT",
        "/api/contexts/e2e-ctx",
        token=config.token_a,
        body={
            "label": "E2E Context",
            "projectPrefixes": ["E2E"],
            "color": "#FF0000",
            "icon": "star",
        },
    )
    record(status == 200, "PUT /api/contexts/{id}", f"PUT /api/contexts/{{id}} (status {status})")

    status, payload = server_json(config, "GET", "/api/contexts", token=config.token_a)
    record(
        status == 200
        and isinstance(payload, list)
        and any(entry.get("id") == "e2e-ctx" for entry in payload),
        "GET /api/contexts includes created context",
        "GET /api/contexts missing created context",
    )

    status, _ = server_request(
        config, "DELETE", "/api/contexts/e2e-ctx", token=config.token_a
    )
    record(
        status == 204,
        "DELETE /api/contexts/{id}",
        f"DELETE /api/contexts/{{id}} (status {status})",
    )

    status, _ = server_request(
        config,
        "PUT",
        "/api/stores/e2e-store",
        token=config.token_a,
        body={"label": "E2E Store", "tag": "e2estore"},
    )
    record(status == 200, "PUT /api/stores/{id}", f"PUT /api/stores/{{id}} (status {status})")

    status, payload = server_json(config, "GET", "/api/stores", token=config.token_a)
    record(
        status == 200
        and isinstance(payload, list)
        and any(entry.get("id") == "e2e-store" for entry in payload),
        "GET /api/stores includes created store",
        "GET /api/stores missing created store",
    )

    status, _ = server_request(
        config, "DELETE", "/api/stores/e2e-store", token=config.token_a
    )
    record(
        status == 204,
        "DELETE /api/stores/{id}",
        f"DELETE /api/stores/{{id}} (status {status})",
    )

    status, _ = server_request(
        config,
        "PUT",
        "/api/presets/e2e-preset",
        token=config.token_a,
        body={"label": "E2E Preset", "rawSuffix": "+e2e project:TEST priority:H"},
    )
    record(
        status == 200,
        "PUT /api/presets/{id}",
        f"PUT /api/presets/{{id}} (status {status})",
    )

    status, _ = server_request(
        config, "DELETE", "/api/presets/e2e-preset", token=config.token_a
    )
    record(
        status == 204,
        "DELETE /api/presets/{id}",
        f"DELETE /api/presets/{{id}} (status {status})",
    )

    status, _ = server_request(
        config,
        "POST",
        "/api/config/geofences",
        token=config.token_a,
        body={
            "version": "1",
            "items": [
                {
                    "id": "home",
                    "name": "Home",
                    "latitude": -33.87,
                    "longitude": 151.21,
                    "radius": 100,
                }
            ],
        },
    )
    record(
        status == 200,
        "POST /api/config/geofences",
        f"POST /api/config/geofences (status {status})",
    )

    status, payload = server_json(config, "GET", "/api/config/geofences", token=config.token_a)
    record(
        status == 200 and isinstance(payload, dict) and isinstance(payload.get("items"), list),
        "GET /api/config/geofences",
    )

    status, _ = server_request(
        config, "DELETE", "/api/config/geofences/home", token=config.token_a
    )
    record(
        status in {200, 204},
        "DELETE /api/config/geofences/{id}",
        f"DELETE /api/config/geofences/{{id}} (status {status})",
    )

    status, _ = server_request(
        config, "GET", "/api/summary", token=config.token_a, timeout=15
    )
    record(status == 200, "GET /api/summary", f"GET /api/summary (status {status})")

    status, _ = server_request(config, "POST", "/api/sync", token=config.token_a)
    record(status == 200, "POST /api/sync (no-op)", f"POST /api/sync (status {status})")

    record(
        wait_for(
            3,
            lambda: "http_requests_total"
            in server_request(config, "GET", "/metrics", timeout=10)[1],
        ),
        "GET /metrics (Prometheus)",
    )

    status, _ = server_request(config, "GET", "/swagger-ui/", timeout=10)
    record(
        status == 200,
        "GET /swagger-ui/",
        f"GET /swagger-ui/ (status {status})",
    )

    status, _ = server_request(config, "GET", "/api-doc/openapi.json", timeout=10)
    record(
        status == 200,
        "GET /api-doc/openapi.json",
        f"GET /api-doc/openapi.json (status {status})",
    )


def run_sync_e2e(config: Config) -> None:
    if not config.sync_enabled:
        skip("TW → REST sync (sync not enabled)")
        skip("REST → TW sync (sync not enabled)")
        skip("Bidirectional mutations (sync not enabled)")
        skip("Multi-device convergence (sync not enabled)")
        return

    if not config.tw_dir_a or not config.tw_dir_b:
        emit("FAIL", "TW sync fixtures not configured")
        return

    tw_run(config, config.tw_dir_a, "add", "+e2e_staging TW-created task project:TWTEST priority:M")
    completed = tw_run(config, config.tw_dir_a, "sync")
    record(
        completed.returncode == 0,
        "task sync succeeds",
        f"task sync failed (exit {completed.returncode})",
    )

    record(
        wait_for(5, lambda: rest_task_exists(config, config.token_b, "TW-created task")),
        "E2E-02: TW task appears in REST API",
        "E2E-02: TW task NOT found in REST API",
    )

    server_request(
        config,
        "POST",
        "/api/tasks",
        token=config.token_b,
        body={"raw": "+e2e_staging REST-to-TW task project:RESTTEST"},
    )
    server_request(config, "GET", "/api/tasks", token=config.token_b)
    time.sleep(2)
    tw_run(config, config.tw_dir_a, "sync")
    record(
        any("REST-to-TW task" in str(task.get("description", "")) for task in tw_export(config, config.tw_dir_a)),
        "E2E-03: REST task appears in TW after sync",
        "E2E-03: REST task NOT found in TW after sync",
    )

    tw_run(config, config.tw_dir_a, "add", "+e2e_staging Bidirectional test task")
    tw_run(config, config.tw_dir_a, "sync")
    time.sleep(1)

    status, rest_tasks = server_json(config, "GET", "/api/tasks", token=config.token_b)
    bidi_uuid = ""
    if status == 200 and isinstance(rest_tasks, list):
        for task in rest_tasks:
            if "Bidirectional" in str(task.get("description", "")):
                bidi_uuid = str(task.get("uuid", ""))
                break

    if not bidi_uuid:
        emit("FAIL", "E2E-04: Could not find bidirectional test task in REST")
    else:
        server_request(
            config,
            "POST",
            f"/api/tasks/{bidi_uuid}/modify",
            token=config.token_b,
            body={"priority": "H"},
        )
        server_request(config, "GET", "/api/tasks", token=config.token_b)
        time.sleep(2)
        tw_run(config, config.tw_dir_a, "sync")
        tw_priority = tw_field_for_uuid(config, config.tw_dir_a, bidi_uuid, "priority")
        record(
            tw_priority == "H",
            "E2E-04: REST modify propagates to TW",
            f"E2E-04: REST modify did not propagate (priority={tw_priority}, expected H)",
        )

        server_request(config, "POST", f"/api/tasks/{bidi_uuid}/done", token=config.token_b)
        server_request(config, "POST", f"/api/tasks/{bidi_uuid}/undo", token=config.token_b)
        server_request(config, "GET", "/api/tasks", token=config.token_b)
        time.sleep(2)
        tw_run(config, config.tw_dir_a, "sync")
        tw_status = tw_field_for_uuid(config, config.tw_dir_a, bidi_uuid, "status")
        record(
            tw_status == "pending",
            "E2E-04: REST undo propagates to TW",
            f"E2E-04: REST undo did not propagate (status={tw_status}, expected pending)",
        )

        tw_run(config, config.tw_dir_a, bidi_uuid, "done")
        tw_run(config, config.tw_dir_a, "sync")
        time.sleep(1)
        server_request(config, "GET", "/api/tasks", token=config.token_b)
        time.sleep(1)
        status, remaining = server_json(config, "GET", "/api/tasks", token=config.token_b)
        if status != 200 or not isinstance(remaining, list):
            emit("FAIL", "E2E-04: TW complete check — REST response was not valid JSON")
        else:
            found = any(task.get("uuid") == bidi_uuid for task in remaining)
            record(
                not found,
                "E2E-04: TW complete propagates to REST (task no longer pending)",
                "E2E-04: TW complete did not propagate — task still pending in REST",
            )

    tw_run(config, config.tw_dir_a, "add", "+e2e_staging Device-A exclusive task")
    tw_run(config, config.tw_dir_a, "sync")
    record(
        wait_for(
            5,
            lambda: tw_sync_has_description(config, config.tw_dir_b, "Device-A exclusive"),
        ),
        "E2E-05: Device B sees Device A's task after sync",
        "E2E-05: Device B does not see Device A's task",
    )

    tw_run(config, config.tw_dir_b, "add", "+e2e_staging Device-B exclusive task")
    tw_run(config, config.tw_dir_b, "sync")
    record(
        wait_for(
            5,
            lambda: tw_sync_has_description(config, config.tw_dir_a, "Device-B exclusive"),
        ),
        "E2E-05: Device A sees Device B's task after sync",
        "E2E-05: Device A does not see Device B's task",
    )

    shared_uuid = tw_uuid_for_description_contains(config, config.tw_dir_a, "Device-A exclusive")
    if not shared_uuid:
        skip("E2E-05: Competing edits (could not find shared task UUID)")
        return

    tw_run(config, config.tw_dir_a, shared_uuid, "modify", "project:CONFLICT_A")
    tw_run(config, config.tw_dir_b, shared_uuid, "modify", "priority:H")
    tw_run(config, config.tw_dir_a, "sync")
    tw_run(config, config.tw_dir_b, "sync")
    tw_run(config, config.tw_dir_a, "sync")
    tw_run(config, config.tw_dir_b, "sync")
    a_desc = tw_field_for_uuid(config, config.tw_dir_a, shared_uuid, "description")
    b_desc = tw_field_for_uuid(config, config.tw_dir_b, shared_uuid, "description")
    record(
        a_desc == b_desc,
        "E2E-05: Devices converge after competing edits",
        f"E2E-05: Devices diverged (A='{a_desc}', B='{b_desc}')",
    )


def run_restart_latency(config: Config) -> None:
    pre_count = task_count(config, config.token_b)
    ssh_run(
        config,
        f"sudo docker compose -f {shlex.quote(DOCKER_COMPOSE_FILE)} restart server",
    )
    time.sleep(5)
    responsive = wait_for(
        20, lambda: server_request(config, "GET", "/healthz", timeout=5)[0] == 200
    )
    record(responsive, "Server responsive after restart")

    post_count = task_count(config, config.token_b)
    record(
        pre_count is not None and pre_count == post_count,
        f"E2E-06: Tasks persist after restart ({pre_count} → {post_count})",
        f"E2E-06: Tasks persist after restart ({pre_count} → {post_count})",
    )

    if config.sync_enabled and config.tw_dir_a:
        tw_run(config, config.tw_dir_a, "add", "+e2e_staging Post-restart task")
        completed = tw_run(config, config.tw_dir_a, "sync")
        record(
            completed.returncode == 0,
            "E2E-06: TW sync works after restart",
            "E2E-06: TW sync failed after restart",
        )

    rest_samples: list[int] = []
    health_samples: list[int] = []
    for _ in range(10):
        start = time.perf_counter()
        server_request(config, "GET", "/api/tasks", token=config.token_a, timeout=10)
        rest_samples.append(int((time.perf_counter() - start) * 1000))

        start = time.perf_counter()
        server_request(config, "GET", "/healthz", timeout=10)
        health_samples.append(int((time.perf_counter() - start) * 1000))

    rest_sorted = sorted(rest_samples)
    health_sorted = sorted(health_samples)
    rest_p50 = rest_sorted[4]
    rest_p95 = rest_sorted[-1]
    health_p50 = health_sorted[4]
    emit("INFO", f"REST GET /api/tasks: p50={rest_p50}ms p95={rest_p95}ms")
    emit("INFO", f"Healthz: p50={health_p50}ms")
    record(
        rest_p95 < 2000,
        f"E2E-10: REST p95 < 2000ms ({rest_p95}ms)",
        f"E2E-10: REST p95 >= 2000ms (got {rest_p95}ms)",
    )
    record(
        health_p50 < 100,
        f"E2E-10: Healthz p50 < 100ms ({health_p50}ms)",
        f"E2E-10: Healthz p50 < 100ms (got {health_p50}ms)",
    )


def main() -> int:
    config = parse_args()
    if config.run_rest_crud:
        run_rest_crud(config)
    if config.run_app_config:
        run_app_config(config)
    if config.run_sync_e2e:
        run_sync_e2e(config)
    if config.run_restart_latency:
        run_restart_latency(config)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
