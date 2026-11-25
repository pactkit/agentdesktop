#!/usr/bin/env python3
"""Smoke-test SQL migrations; does not exercise the SQLx migration ledger."""
from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = ROOT / "crates/controller/migrations"


def migration_files():
    files = sorted(MIGRATIONS.glob("*.sql"))
    versions = [int(re.match(r"^(\d+)_", path.name).group(1)) for path in files]
    if len(versions) != len(set(versions)):
        raise AssertionError("duplicate SQLx migration version")
    return files


def apply(connection, files):
    for path in files:
        connection.executescript(path.read_text(encoding="utf-8"))


class MigrationSmoke(unittest.TestCase):
    def connect(self):
        connection = sqlite3.connect(":memory:")
        self.addCleanup(connection.close)
        connection.execute("PRAGMA foreign_keys = ON")
        return connection

    def assert_integrity(self, connection):
        self.assertEqual(connection.execute("PRAGMA foreign_key_check").fetchall(), [])
        self.assertEqual(connection.execute("PRAGMA integrity_check").fetchone(), ("ok",))

    def test_fresh_database(self):
        connection = self.connect()
        apply(connection, migration_files())
        self.assert_integrity(connection)
        names = {
            row[0]
            for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")
        }
        self.assertTrue({
            "enrollment_receipts", "device_agent_state", "agent_jobs",
            "agent_install_receipts", "audit_events",
        } <= names)

    def test_upgrade_preserves_enrollment_and_audit(self):
        connection = self.connect()
        files = migration_files()
        baseline = [p for p in files if int(p.name.split("_")[0]) <= 6]
        features = [p for p in files if 7 <= int(p.name.split("_")[0]) <= 9]
        later = [p for p in files if int(p.name.split("_")[0]) >= 10]
        apply(connection, baseline)
        connection.execute(
            "INSERT INTO devices (id, hostname, created_at, enrolled_by_subject, "
            "enrollment_token_id) VALUES ('d', 'host', 1, 'user', 'token')"
        )
        connection.execute("INSERT INTO enrollment_receipts VALUES ('d', '{}')")
        apply(connection, features)
        connection.execute(
            "INSERT INTO audit_events (id, actor, action, device_id, payload_json, created_at) "
            "VALUES ('a', 'admin', 'test', 'd', '{}', 1)"
        )
        apply(connection, later)
        self.assertEqual(
            connection.execute("SELECT state, enrollment_token_id FROM devices").fetchone(),
            ("active", "token"),
        )
        self.assertEqual(
            connection.execute("SELECT subject FROM audit_events").fetchone(), ("user",)
        )
        self.assertEqual(
            connection.execute("SELECT count(*) FROM enrollment_receipts").fetchone(), (1,)
        )
        self.assert_integrity(connection)


if __name__ == "__main__":
    unittest.main()
