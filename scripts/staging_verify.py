#!/usr/bin/env python3
"""Structured staging verification runner.

This runner owns argument parsing, environment discovery, and preflight checks.
It then delegates the detailed scenario bodies to the transitional shell runner
at `scripts/staging_verify_legacy.sh`.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import ssl
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path


GREEN = "\033[0;32m"
RED = "\033[0;31m"
CYAN = "\033[0;36m"
YELLOW = "\033[1;33m"
BOLD = "\033[1m"
NC = "\033[0m"

DOCKER_COMPOSE_FILE = "/opt/cmdock/docker-compose.yml"
REMOTE_CMDOCK_ADMIN = "/usr/local/bin/cmdock-admin"
ANSIBLE_INVENTORY_FILE = "deploy/ansible/inventory/hosts.yml"
ANSIBLE_STAGING_HOST = "vm-cmdock-staging-01"
ANSIBLE_DOGFOOD_HOST = "vm-cmdock-dogfood-01"


@dataclass
class Config:
    run_full: bool
    require_admin_http: bool
    server_url: str
    ssh_host: str
    tw_exec_mode: str
    tw_host: str
    admin_http_token: str
    legacy_args: list[str]


def parse_args() -> Config:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--full", action="store_true")
    parser.add_argument("--require-admin-http", action="store_true")
    parser.add_argument("--url")
    parser.add_argument("--ssh")
    parser.add_argument("--tw-host")
    parser.add_argument("--tw-local", action="store_true")
    parser.add_argument("--help", action="help")
    args = parser.parse_args()

    server_url = args.url or os.environ.get(
        "STAGING_URL",
        os.environ.get("CMDOCK_STAGING_URL", "https://staging.example.com"),
    )
    ssh_host = args.ssh or os.environ.get(
        "STAGING_SSH",
        os.environ.get("CMDOCK_STAGING_SSH", "staging.example.com"),
    )
    tw_exec_mode = "local" if args.tw_local else os.environ.get("STAGING_TW_MODE", "ssh")
    tw_host = args.tw_host or os.environ.get("STAGING_TW_HOST") or ssh_host
    admin_http_token = os.environ.get("STAGING_ADMIN_TOKEN") or os.environ.get(
        "CMDOCK_ADMIN_TOKEN", ""
    )

    legacy_args: list[str] = []
    if args.full:
        legacy_args.append("--full")
    if args.require_admin_http:
        legacy_args.append("--require-admin-http")
    if args.url:
        legacy_args.extend(["--url", args.url])
    if args.ssh:
        legacy_args.extend(["--ssh", args.ssh])
    if args.tw_host:
        legacy_args.extend(["--tw-host", args.tw_host])
    if args.tw_local:
        legacy_args.append("--tw-local")

    return Config(
        run_full=args.full,
        require_admin_http=args.require_admin_http,
        server_url=server_url,
        ssh_host=ssh_host,
        tw_exec_mode=tw_exec_mode,
        tw_host=tw_host,
        admin_http_token=admin_http_token,
        legacy_args=legacy_args,
    )


def print_banner(config: Config) -> None:
    mode = "Full (P0+P1)" if config.run_full else "P0 only"
    admin_mode = "required" if config.require_admin_http else "optional"
    tw_mode = config.tw_exec_mode
    if tw_mode == "ssh":
        tw_mode = f"{tw_mode} ({config.tw_host})"

    print(f"{BOLD}╔══════════════════════════════════════════════════════╗{NC}")
    print(f"{BOLD}║   cmdock-server — Internal E2E Tests                 ║{NC}")
    print(f"{BOLD}╚══════════════════════════════════════════════════════╝{NC}")
    print(f"  Server:  {CYAN}{config.server_url}{NC}")
    print(f"  SSH:     {CYAN}{config.ssh_host}{NC}")
    print(f"  TW:      {CYAN}{tw_mode}{NC}")
    print(f"  Mode:    {CYAN}{mode}{NC}")
    print(f"  Admin:   {CYAN}{admin_mode}{NC}")
    print()


def pass_line(message: str) -> None:
    print(f"  {GREEN}✓{NC} {message}")


def skip_line(message: str) -> None:
    print(f"  {YELLOW}○{NC} {message} (skipped)")


def warn_line(message: str) -> None:
    print(f"  {YELLOW}!{NC} {message}")


def fatal(message: str, detail: str | None = None) -> None:
    print(f"{RED}{message}{NC}", file=sys.stderr)
    if detail:
        print(detail, file=sys.stderr)
    raise SystemExit(1)


def run_command(
    args: list[str],
    *,
    check: bool = True,
    capture_output: bool = True,
    text: bool = True,
) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        args,
        check=False,
        capture_output=capture_output,
        text=text,
    )
    if check and completed.returncode != 0:
        raise subprocess.CalledProcessError(
            completed.returncode, args, completed.stdout, completed.stderr
        )
    return completed


def ssh_command(
    config: Config,
    remote_command: str,
    *,
    timeout: int = 10,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    return run_command(
        [
            "ssh",
            "-o",
            "ControlMaster=no",
            "-o",
            f"ConnectTimeout={timeout}",
            config.ssh_host,
            remote_command,
        ],
        check=check,
    )


def tw_remote_ready(config: Config) -> bool:
    return (
        run_command(
            [
                "ssh",
                "-o",
                "ControlMaster=no",
                "-o",
                "ConnectTimeout=10",
                config.tw_host,
                "command -v task >/dev/null 2>&1 && task --version >/dev/null 2>&1",
            ],
            check=False,
        ).returncode
        == 0
    )


def load_admin_token_from_ansible(config: Config) -> str:
    if config.admin_http_token:
        return config.admin_http_token
    if shutil.which("ansible-inventory") is None:
        return ""

    inventory_path = Path(ANSIBLE_INVENTORY_FILE)
    if not inventory_path.exists():
        return ""

    inventory_host = ANSIBLE_DOGFOOD_HOST if (
        "dogfood" in config.ssh_host or "taskchampion-01" in config.ssh_host
    ) else ANSIBLE_STAGING_HOST
    completed = run_command(
        [
            "ansible-inventory",
            "-i",
            str(inventory_path),
            "--host",
            inventory_host,
        ],
        check=False,
    )
    if completed.returncode != 0:
        return ""
    try:
        data = json.loads(completed.stdout)
    except json.JSONDecodeError:
        return ""
    token = data.get("cmdock_admin_token", "")
    if token:
        print(
            f"{BOLD}==> {NC}Loaded admin HTTP token from local Ansible inventory for {inventory_host}"
        )
    return token


def http_ok(url: str) -> bool:
    request = urllib.request.Request(url, headers={"User-Agent": "cmdock-staging-verify"})
    try:
        with urllib.request.urlopen(request, timeout=5, context=ssl.create_default_context()):
            return True
    except urllib.error.URLError:
        return False


def parse_created_timestamp(value: str) -> datetime | None:
    if not value or value == "unknown":
        return None
    value = value.strip()
    if value.endswith("Z"):
        value = value[:-1] + "+00:00"
    try:
        return datetime.fromisoformat(value)
    except ValueError:
        return None


def validate_dependencies(config: Config) -> None:
    required = ["curl", "jq", "ssh", "bc"]
    if config.tw_exec_mode == "local":
        required.append("task")

    for command in required:
        if shutil.which(command) is None:
            fatal(f"Missing dependency: {command}")


def run_preflight(config: Config) -> tuple[str, bool]:
    validate_dependencies(config)

    print(f"{BOLD}==> {NC}Checking connectivity...")
    if not http_ok(f"{config.server_url}/healthz"):
        fatal(
            f"Server unreachable at {config.server_url}",
            "  Is the staging server running? Try: ./scripts/deploy.sh health",
        )
    pass_line("Server reachable (healthz)")

    remote_health = (
        f"sudo docker compose -f '{DOCKER_COMPOSE_FILE}' ps server 2>/dev/null | grep -q healthy"
    )
    if ssh_command(config, remote_health, timeout=5, check=False).returncode != 0:
        fatal(f"Cannot SSH to {config.ssh_host} or server container not healthy")
    pass_line("SSH access + server container healthy")

    if config.tw_exec_mode == "ssh":
        if not tw_remote_ready(config):
            fatal(f"Cannot run Taskwarrior on {config.tw_host}")
        pass_line("SSH access + remote Taskwarrior client ready")
    else:
        pass_line("Local Taskwarrior client ready")

    config.admin_http_token = load_admin_token_from_ansible(config)
    if not config.admin_http_token and config.require_admin_http:
        fatal(
            "Admin HTTP coverage required but no admin token is available.",
            "  Provide STAGING_ADMIN_TOKEN / CMDOCK_ADMIN_TOKEN or ensure local Ansible inventory exposes cmdock_admin_token.",
        )
    if not config.admin_http_token and config.run_full:
        warn_line("Full mode is running without admin HTTP coverage.")
        print(
            "    /admin/* tests will be skipped unless STAGING_ADMIN_TOKEN / CMDOCK_ADMIN_TOKEN is set"
        )
        print("    or local Ansible inventory exposes cmdock_admin_token.")

    standalone_cli_ready = False
    if config.admin_http_token:
        remote_cmd = (
            f"command -v cmdock-admin >/dev/null 2>&1 && {shlex.quote(REMOTE_CMDOCK_ADMIN)} --version >/dev/null 2>&1"
        )
        if ssh_command(config, remote_cmd, timeout=5, check=False).returncode != 0:
            fatal(
                f"Standalone cmdock-admin is not installed on {config.ssh_host}",
                "  Deploy it first from the cmdock/cli repo: just deploy-staging",
            )
        standalone_cli_ready = True
        pass_line("SSH access + standalone cmdock-admin ready")
    else:
        skip_line("Standalone cmdock-admin coverage (no admin token available)")

    print(f"{BOLD}==> {NC}Validating Docker image...")
    remote_created = ssh_command(
        config,
        "sudo docker inspect cmdock-server:latest --format '{{.Created}}'",
        timeout=5,
        check=False,
    ).stdout.strip() or "unknown"
    local_created = (
        run_command(
            ["docker", "inspect", "cmdock-server:latest", "--format", "{{.Created}}"],
            check=False,
        ).stdout.strip()
        or "unknown"
    )
    remote_dt = parse_created_timestamp(remote_created)
    local_dt = parse_created_timestamp(local_created)
    if remote_dt is None:
        skip_line("Docker image age check (cannot inspect remote image)")
    else:
        if local_dt is not None:
            age_diff_seconds = abs(int((local_dt - remote_dt).total_seconds()))
            if age_diff_seconds > 3600:
                warn_line(f"Staging image is {age_diff_seconds // 60}m older than local build")
                print(f"    Local:  {local_created}")
                print(f"    Remote: {remote_created}")
                print(
                    f"  Consider redeploying: docker save cmdock-server:latest | ssh {config.ssh_host} 'sudo docker load'"
                )
        pass_line(f"Docker image validated (remote: {remote_created[:19]})")

    bubble_dir = tempfile.mkdtemp(prefix="staging-test-")
    return bubble_dir, standalone_cli_ready


def run_legacy(config: Config, bubble_dir: str, standalone_cli_ready: bool) -> int:
    script_dir = Path(__file__).resolve().parent
    legacy_script = script_dir / "staging_verify_legacy.sh"
    env = os.environ.copy()
    env["CMDOCK_STAGING_SKIP_PREFLIGHT"] = "true"
    env["CMDOCK_STAGING_BUBBLE_DIR"] = bubble_dir
    env["CMDOCK_STAGING_STANDALONE_CLI_READY"] = "true" if standalone_cli_ready else "false"
    if config.admin_http_token:
        env["ADMIN_HTTP_TOKEN"] = config.admin_http_token
        env["CMDOCK_ADMIN_TOKEN"] = config.admin_http_token
    return subprocess.run([str(legacy_script), *config.legacy_args], env=env, check=False).returncode


def main() -> int:
    config = parse_args()
    print_banner(config)
    bubble_dir, standalone_cli_ready = run_preflight(config)
    return run_legacy(config, bubble_dir, standalone_cli_ready)


if __name__ == "__main__":
    raise SystemExit(main())
