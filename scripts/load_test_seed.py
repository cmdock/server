#!/usr/bin/env python3
"""Seed load-test users, devices, replicas, and token records."""

from __future__ import annotations

import argparse
import binascii
import hashlib
import os
import sqlite3
import uuid
from base64 import b64encode
from pathlib import Path

from cryptography.hazmat.primitives.ciphers.aead import AESGCM


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db-path", required=True)
    parser.add_argument("--server-data", required=True)
    parser.add_argument("--tokens-file", required=True)
    parser.add_argument("--master-key", required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--users", type=int, required=True)
    parser.add_argument("--personal-count", type=int, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    conn = sqlite3.connect(args.db_path)
    server_data = Path(args.server_data)
    tokens: list[str] = []

    master_key = bytes.fromhex(args.master_key)
    aesgcm = AESGCM(master_key)

    def encrypt_secret(raw_bytes: bytes) -> str:
        nonce = os.urandom(12)
        ciphertext_and_tag = aesgcm.encrypt(nonce, raw_bytes, None)
        return b64encode(nonce + ciphertext_and_tag).decode()

    def add_token_and_device(user_id: str, token_type: str, device_name: str) -> None:
        token = f"{token_type}-token-{uuid.uuid4().hex[:8]}"
        token_hash = hashlib.sha256(token.encode()).hexdigest()
        client_id = str(uuid.uuid4())

        device_secret = os.urandom(32)
        device_secret_hex = binascii.hexlify(device_secret).decode()
        device_secret_enc_b64 = encrypt_secret(device_secret)

        conn.execute(
            "INSERT INTO api_tokens (token_hash, user_id, label) VALUES (?, ?, 'load-test')",
            (token_hash, user_id),
        )
        conn.execute(
            "INSERT INTO devices (client_id, user_id, name, encryption_secret_enc, status) VALUES (?, ?, ?, ?, 'active')",
            (client_id, user_id, device_name, device_secret_enc_b64),
        )

        tokens.append(f"{token_type}:{token}:{client_id}:{device_secret_hex}")

    def create_account(user_id: str, username: str) -> None:
        replica_id = str(uuid.uuid4())
        replica_secret = os.urandom(32)
        replica_secret_enc_b64 = encrypt_secret(replica_secret)

        conn.execute(
            "INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'not-real')",
            (user_id, username),
        )
        conn.execute(
            "INSERT INTO replicas (id, user_id, encryption_secret_enc) VALUES (?, ?, ?)",
            (replica_id, user_id, replica_secret_enc_b64),
        )
        (server_data / "users" / user_id).mkdir(parents=True, exist_ok=True)

    profile = args.profile
    if profile == "mixed":
        for index in range(1, args.personal_count + 1):
            user_id = f"personal-user-{index}"
            username = f"personal{index}"
            create_account(user_id, username)
            add_token_and_device(user_id, "personal", f"{username} load device")

        create_account("team-shared-user", "teamuser")
        add_token_and_device("team-shared-user", "team", "team shared load device")
    elif profile == "personal-only":
        for index in range(1, args.users + 1):
            user_id = f"personal-user-{index}"
            username = f"personal{index}"
            create_account(user_id, username)
            add_token_and_device(user_id, "personal", f"{username} load device")
    elif profile == "team-contention":
        create_account("team-shared-user", "teamuser")
        add_token_and_device("team-shared-user", "team", "team shared load device")
    elif profile == "multi-device-single-user":
        create_account("multi-device-user", "multidevice")
        for index in range(1, args.users + 1):
            add_token_and_device(
                "multi-device-user",
                "multi_device",
                f"multidevice load device {index}",
            )
    else:
        raise SystemExit(f"unsupported profile: {profile}")

    conn.commit()
    conn.close()

    Path(args.tokens_file).write_text("".join(f"{entry}\n" for entry in tokens))
    print(f"Created {len(tokens)} load-test token entries for profile {profile}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
