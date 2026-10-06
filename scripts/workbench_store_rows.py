"""The workbench's durable rows, read the same way whichever store it runs on.

A SQLite workbench's rows are read from its data directory. A PostgreSQL
workbench's rows come from the case controller, which owns that database's
lease (`store-rows` over H6_CONTROL) and answers them in the shape SQLite does.
Attachment bytes live in SQLite's `attachment_blobs` table, or in the file
attachment store PostgreSQL workbenches keep beside their data.
"""
from __future__ import annotations

import sqlite3
from pathlib import Path
from typing import Any, Callable

STORES = ("sqlite_file", "postgresql")


class StoreRows:
    def __init__(self, store: str, data: Path, controller: Callable[[dict], dict] | None = None):
        assert store in STORES, store
        assert store == "sqlite_file" or controller is not None, "PostgreSQL rows are read through the controller"
        self.store = store
        self.data = data
        self.controller = controller

    @property
    def database(self) -> Path:
        return self.data / "lash-sessions.db"

    def sql(self, query: str, params: tuple = ()) -> list[dict[str, Any]]:
        with sqlite3.connect(f"file:{self.database}?mode=ro", uri=True) as connection:
            connection.row_factory = sqlite3.Row
            return [dict(row) for row in connection.execute(query, params)]

    def rows(self, table: str, order: tuple[str, ...] = (), **equal: str) -> list[dict[str, Any]]:
        """Every row of `table` whose `equal` columns match, sorted by `order`."""
        if self.store == "sqlite_file":
            where = " AND ".join(f"{column} = ?" for column in equal)
            rows = self.sql(f"SELECT * FROM {table} WHERE {where}", tuple(equal.values()))
        else:
            rows = self.controller({"action": "store-rows", "table": table, "equal": equal})["rows"]
        return sorted(rows, key=lambda row: tuple(row[column] for column in order)) if order else rows

    def attachment(self, attachment_id: str) -> list[bytes]:
        """The stored bytes of one attachment: one entry when it is stored."""
        if self.store == "sqlite_file":
            return [row["content"] for row in
                    self.sql("SELECT content FROM attachment_blobs WHERE attachment_id = ?", (attachment_id,))]
        path = self.data / "attachments/blake3" / attachment_id[:2] / attachment_id
        return [path.read_bytes()] if path.is_file() else []
