#!/usr/bin/env python3
"""Smoke test: official Google SDK against digital twin servers.

Verifies that google-api-python-client can discover and call the Drive and
Gmail twins via their served discovery documents, with zero monkey-patching.

Usage:
    # Start twins first:
    #   cargo run --bin twin-drive-server   (port 9100)
    #   cargo run --bin twin-gmail-server   (port 9200)
    python tests/sdk_smoke/test_google_sdk.py

Requires:
    pip install google-api-python-client google-auth
"""

import json
import sys
import urllib.request

from googleapiclient.discovery import build
from google.auth.credentials import AnonymousCredentials
from google.auth.transport.requests import Request

DRIVE_URL = "http://localhost:9100"
GMAIL_URL = "http://localhost:9200"

PASS = 0
FAIL = 0


def check(label: str, condition: bool, detail: str = ""):
    global PASS, FAIL
    if condition:
        PASS += 1
        print(f"  [PASS] {label}")
    else:
        FAIL += 1
        print(f"  [FAIL] {label}: {detail}")


def reset_twin(base_url: str):
    """Reset the twin to a clean state."""
    data = json.dumps({"seed": 42, "start_time_unix_ms": 1704067200000}).encode()
    req = urllib.request.Request(
        f"{base_url}/control/reset",
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    urllib.request.urlopen(req)


def seed_drive(base_url: str):
    """Seed the Drive twin with a scenario giving the default actor ownership."""
    scenario = {
        "version": 1,
        "name": "sdk-smoke-setup",
        "seed": 42,
        "start_time_unix_ms": 1704067200000,
        "actors": [{"id": "default", "label": "Default User"}],
        "initial_state": {
            "files": [
                {
                    "id": "root",
                    "name": "My Drive",
                    "parent_id": None,
                    "owner_id": "default",
                    "kind": "Folder",
                }
            ]
        },
        "timeline": [],
        "faults": [],
        "assertions": [],
    }
    data = json.dumps(scenario).encode()
    req = urllib.request.Request(
        f"{base_url}/control/scenario/apply",
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    urllib.request.urlopen(req)


def test_discovery_fetch():
    """Test that discovery documents are fetchable."""
    print("\n--- Discovery document fetch ---")

    # Drive V2-style
    resp = urllib.request.urlopen(f"{DRIVE_URL}/$discovery/rest?version=v3")
    doc = json.loads(resp.read())
    check("Drive discovery name", doc["name"] == "drive", doc.get("name"))
    check("Drive discovery version", doc["version"] == "v3", doc.get("version"))
    check("Drive rootUrl set", doc["rootUrl"].startswith("http://"), doc.get("rootUrl"))
    check("Drive has files resource", "files" in doc.get("resources", {}))

    # Gmail V1-style
    resp = urllib.request.urlopen(f"{GMAIL_URL}/discovery/v1/apis/gmail/v1/rest")
    doc = json.loads(resp.read())
    check("Gmail discovery name", doc["name"] == "gmail", doc.get("name"))
    check("Gmail discovery version", doc["version"] == "v1", doc.get("version"))
    check("Gmail has users resource", "users" in doc.get("resources", {}))


def test_drive_sdk():
    """Test Drive twin with official google-api-python-client."""
    print("\n--- Drive SDK smoke test ---")
    seed_drive(DRIVE_URL)

    # Build the Drive service using discovery document from the twin
    service = build(
        "drive",
        "v3",
        discoveryServiceUrl=f"{DRIVE_URL}/$discovery/rest?version=v3",
        credentials=AnonymousCredentials(),
        static_discovery=False,
    )

    # List files (should be empty except root)
    result = service.files().list().execute()
    check("files.list returns dict", isinstance(result, dict))
    files = result.get("files", [])
    check("files.list returns files array", isinstance(files, list))

    # Create a folder
    folder_meta = {
        "name": "SDK Test Folder",
        "mimeType": "application/vnd.google-apps.folder",
    }
    folder = service.files().create(body=folder_meta).execute()
    check("create folder returns id", "id" in folder, str(folder))
    folder_id = folder["id"]
    check("folder name matches", folder.get("name") == "SDK Test Folder")

    # Create a file inside the folder
    file_meta = {
        "name": "test.txt",
        "parents": [folder_id],
    }
    created = service.files().create(body=file_meta).execute()
    check("create file returns id", "id" in created)
    file_id = created["id"]

    # Get the file
    fetched = service.files().get(fileId=file_id).execute()
    check("get file returns correct name", fetched.get("name") == "test.txt")

    # List files with query
    result = service.files().list(q=f"'{folder_id}' in parents").execute()
    ids = [f["id"] for f in result.get("files", [])]
    check("list with parent query finds file", file_id in ids, str(ids))

    # Update (rename) the file
    updated = (
        service.files().update(fileId=file_id, body={"name": "renamed.txt"}).execute()
    )
    check("update returns renamed file", updated.get("name") == "renamed.txt")

    # Delete the file
    service.files().delete(fileId=file_id).execute()
    result = service.files().list(q=f"'{folder_id}' in parents").execute()
    ids = [f["id"] for f in result.get("files", [])]
    check("delete removes file from listing", file_id not in ids, str(ids))


def test_gmail_sdk():
    """Test Gmail twin with official google-api-python-client."""
    print("\n--- Gmail SDK smoke test ---")
    reset_twin(GMAIL_URL)

    service = build(
        "gmail",
        "v1",
        discoveryServiceUrl=f"{GMAIL_URL}/$discovery/rest?version=v1",
        credentials=AnonymousCredentials(),
        static_discovery=False,
    )

    # Get profile
    profile = service.users().getProfile(userId="me").execute()
    check("getProfile returns dict", isinstance(profile, dict))

    # List labels
    labels_result = service.users().labels().list(userId="me").execute()
    check("labels.list returns dict", isinstance(labels_result, dict))
    labels = labels_result.get("labels", [])
    check("has system labels", len(labels) > 0, f"got {len(labels)} labels")

    # Create a label
    new_label = (
        service.users()
        .labels()
        .create(
            userId="me",
            body={"name": "SDK-Test-Label"},
        )
        .execute()
    )
    check("create label returns id", "id" in new_label, str(new_label))
    label_id = new_label["id"]

    # Get the label
    fetched_label = service.users().labels().get(userId="me", id=label_id).execute()
    check("get label name matches", fetched_label.get("name") == "SDK-Test-Label")

    # Send a message (the twin accepts a simplified JSON body)
    import base64

    raw_message = base64.urlsafe_b64encode(
        b"From: test@example.com\r\n"
        b"To: recipient@example.com\r\n"
        b"Subject: SDK Smoke Test\r\n"
        b"\r\n"
        b"Hello from the official SDK!"
    ).decode("ascii")
    sent = (
        service.users()
        .messages()
        .send(
            userId="me",
            body={"raw": raw_message},
        )
        .execute()
    )
    check("send returns id", "id" in sent, str(sent))
    msg_id = sent["id"]

    # List messages
    msgs_result = service.users().messages().list(userId="me").execute()
    check("messages.list returns dict", isinstance(msgs_result, dict))
    messages = msgs_result.get("messages", [])
    check("sent message in listing", any(m["id"] == msg_id for m in messages))

    # Get the message
    msg = service.users().messages().get(userId="me", id=msg_id).execute()
    check("get message returns id", msg.get("id") == msg_id)

    # List threads
    threads_result = service.users().threads().list(userId="me").execute()
    check("threads.list returns dict", isinstance(threads_result, dict))
    threads = threads_result.get("threads", [])
    check("has at least one thread", len(threads) > 0)

    # Trash and untrash
    trashed = service.users().messages().trash(userId="me", id=msg_id).execute()
    check("trash returns message", "id" in trashed)

    untrashed = service.users().messages().untrash(userId="me", id=msg_id).execute()
    check("untrash returns message", "id" in untrashed)

    # Delete label
    service.users().labels().delete(userId="me", id=label_id).execute()
    labels_after = service.users().labels().list(userId="me").execute()
    label_ids = [l["id"] for l in labels_after.get("labels", [])]
    check("deleted label gone", label_id not in label_ids)


def main():
    global PASS, FAIL

    # Check twins are reachable
    for name, url in [("Drive", DRIVE_URL), ("Gmail", GMAIL_URL)]:
        try:
            urllib.request.urlopen(f"{url}/health")
        except Exception as e:
            print(f"ERROR: {name} twin not reachable at {url}: {e}")
            print(f"Start it with: cargo run --bin twin-{name.lower()}-server")
            sys.exit(1)

    test_discovery_fetch()
    test_drive_sdk()
    test_gmail_sdk()

    print(f"\n{'=' * 50}")
    print(f"Results: {PASS} passed, {FAIL} failed")
    if FAIL > 0:
        print("SOME TESTS FAILED")
        sys.exit(1)
    else:
        print("ALL TESTS PASSED")
        sys.exit(0)


if __name__ == "__main__":
    main()
