"""PDFQ client example: sync compile, sync upload, async job with webhook.

Requires Python 3.10+ and `requests`. Run from the repository root:

    pip install requests
    python examples/clients/python/pdfq_example.py

PDFs are written to the current directory.
"""

import hashlib
import hmac
import json
import os
import queue
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import requests

BASE_URL = os.environ.get("PDFQ_URL", "http://localhost:3000").rstrip("/")
SECRET = os.environ.get("PDFQ_WEBHOOK_SECRET", "dev-secret")
# Host the PDFQ container uses to reach this script.
CALLBACK_HOST = os.environ.get("PDFQ_CALLBACK_HOST", "host.docker.internal")
CALLBACK_PORT = int(os.environ.get("PDFQ_CALLBACK_PORT", "8765"))
INVOICE_DATA = Path(__file__).resolve().parents[2] / "invoice" / "data.json"


def print_error(resp: requests.Response) -> None:
    """Print a PDFQ error body: {"error": {"kind", "message", "diagnostics"}}."""
    err = resp.json()["error"]
    print(f"  HTTP {resp.status_code} {err['kind']}: {err['message']}")
    for d in err.get("diagnostics", []):
        print(f"    {d.get('file')}:{d.get('line')}:{d.get('column')} {d['message']}")


def save(name: str, pdf: bytes) -> None:
    Path(name).write_bytes(pdf)
    print(f"  wrote {name} ({len(pdf)} bytes, starts with {pdf[:4]!r})")


def sync_template() -> None:
    """POST /compile with a JSON body: mounted template + data."""
    print("sync: template 'invoice'")
    data = json.loads(INVOICE_DATA.read_text())
    resp = requests.post(f"{BASE_URL}/compile", json={"template": "invoice", "data": data})
    if resp.ok:
        save("invoice.pdf", resp.content)
        print(f"  pages: {resp.headers['X-Pdfq-Pages']}")
    else:
        print_error(resp)


def sync_upload() -> None:
    """POST /compile multipart: the filename is the path in the project."""
    print("sync: uploaded source")
    source = '= Hello #sys.inputs.at("who", default: "world")!\n'
    resp = requests.post(
        f"{BASE_URL}/compile",
        files={"file": ("main.typ", source, "text/plain")},
        data={"input.who": "PDFQ"},
    )
    if resp.ok:
        save("hello.pdf", resp.content)
    else:
        print_error(resp)


def verify(secret: bytes, headers, body: bytes, max_age: int = 300) -> bool:
    """Check X-Pdfq-Signature: sha256=hex(HMAC-SHA256(secret, "<timestamp>.<body>"))."""
    ts = headers.get("X-Pdfq-Timestamp", "")
    signature = headers.get("X-Pdfq-Signature", "")
    if not ts.isdigit() or abs(time.time() - int(ts)) > max_age:
        return False  # missing or stale timestamp: possible replay
    expected = "sha256=" + hmac.new(secret, ts.encode() + b"." + body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(signature.encode(), expected.encode())


def start_receiver(deliveries: queue.Queue) -> HTTPServer:
    """Webhook receiver on 0.0.0.0:CALLBACK_PORT, in a background thread."""

    class Hook(BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            ok = verify(SECRET.encode(), self.headers, body)
            # Reply fast; any 2xx is a delivery, 401 is final (no retry).
            self.send_response(204 if ok else 401)
            self.end_headers()
            deliveries.put((ok, self.headers, body))  # case-insensitive headers

        def log_message(self, *args):
            pass  # keep the output readable

    server = HTTPServer(("0.0.0.0", CALLBACK_PORT), Hook)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def async_webhook() -> None:
    """POST /jobs with callback_url, then wait for the signed webhook."""
    print("async: job with webhook")
    deliveries: queue.Queue = queue.Queue()
    server = start_receiver(deliveries)
    try:
        callback = f"http://{CALLBACK_HOST}:{CALLBACK_PORT}/hook"
        resp = requests.post(
            f"{BASE_URL}/jobs",
            files={"file": ("main.typ", "= Async job\nDelivered by webhook.\n")},
            data={"callback_url": callback},
        )
        if resp.status_code != 202:
            return print_error(resp)
        job_id = resp.json()["job_id"]
        print(f"  queued {job_id}, waiting for {callback}")
        ok, headers, body = deliveries.get(timeout=60)
        if not ok:
            print("  signature REJECTED")
        elif headers["X-Pdfq-Status"] == "succeeded":
            print("  signature ok")
            save("async.pdf", body)
        else:
            print(f"  signature ok, job failed: {json.loads(body)['error']}")
    finally:
        server.shutdown()


def async_polling() -> None:
    """POST /jobs without callback_url, poll GET /jobs/{id}, fetch the PDF."""
    print("async: job with polling")
    resp = requests.post(
        f"{BASE_URL}/jobs",
        files={"file": ("main.typ", "= Async job\nFetched by polling.\n")},
    )
    if resp.status_code != 202:
        return print_error(resp)
    status_url = BASE_URL + resp.json()["status_url"]
    while (job := requests.get(status_url).json())["status"] in ("queued", "running"):
        time.sleep(0.5)
    print(f"  status: {job['status']}")
    pdf = requests.get(f"{status_url}/pdf")
    if pdf.ok:
        save("polled.pdf", pdf.content)
    else:
        print_error(pdf)


if __name__ == "__main__":
    sync_template()
    sync_upload()
    # Webhooks need PDFQ_WEBHOOK_SECRET on both sides; poll otherwise.
    if SECRET:
        async_webhook()
    async_polling()
