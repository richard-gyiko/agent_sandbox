"""Shared HTTP helpers for twin communication."""

from __future__ import annotations

import json
from typing import Any
from urllib.request import Request, urlopen


def get_json(url: str) -> Any:
    req = Request(url, method="GET")
    with urlopen(req) as response:
        return json.loads(response.read().decode("utf-8"))


def post_json(url: str, payload: dict[str, Any]) -> dict[str, Any]:
    req = Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(req) as response:
        return json.loads(response.read().decode("utf-8"))
