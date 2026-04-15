# SDK Smoke Tests

Python smoke tests that verify the digital twins work with official Google SDKs
using dynamically-served discovery documents. Zero monkey-patching required.

## Prerequisites

```bash
pip install google-api-python-client google-auth
```

## Running

Start the twins first:

```bash
cargo run --bin twin-drive-server   # listens on :9100
cargo run --bin twin-gmail-server   # listens on :9200
```

Then run the smoke test:

```bash
python tests/sdk_smoke/test_google_sdk.py
```

The script exits 0 on success, non-zero on failure.
