# Client examples

Small clients for the [PDFQ API](../../docs/api-spec.md), each showing the same scenarios: compile a mounted template with JSON data, compile an uploaded Typst file, run an async job with a signed webhook or with polling.

| Client | Requirements |
|---|---|
| [`python/pdfq_example.py`](python/pdfq_example.py) | Python 3.10+, `requests` |
| [`node/pdfq_example.mjs`](node/pdfq_example.mjs) | Node.js 18+, no dependencies |
| [`livebook/pdfq.livemd`](livebook/pdfq.livemd) | [Livebook](https://livebook.dev) (Elixir, `req` and `kino`); polling only, plus a signature check function |

## Run the server

From the repository root, so that `examples/invoice` is mounted as the `invoice` template:

```sh
docker run --rm -p 3000:3000 -v "$PWD/examples:/templates:ro" \
  -e PDFQ_WEBHOOK_SECRET=dev-secret \
  -e PDFQ_WEBHOOK_ALLOW_HTTP=true -e PDFQ_WEBHOOK_ALLOW_PRIVATE=true \
  ghcr.io/nseaseb/pdfq:0.1.0-typst0.15.1
```

To run your own build instead, `docker build -t pdfq .` and replace the image name with `pdfq`.

> **Local testing only.** `PDFQ_WEBHOOK_ALLOW_HTTP` and `PDFQ_WEBHOOK_ALLOW_PRIVATE` let callbacks reach plain-HTTP receivers on your machine. In production, keep the defaults (HTTPS, public addresses) or restrict hosts with `PDFQ_WEBHOOK_ALLOWED_HOSTS`.

The webhook receivers listen on port `8765` of your machine. From inside the container, your machine is `host.docker.internal`: Docker Desktop provides it; on Linux, add `--add-host=host.docker.internal:host-gateway` to `docker run`.

## Run the clients

Run them from the repository root; PDFs are written to the current directory.

```sh
pip install requests && python examples/clients/python/pdfq_example.py
node examples/clients/node/pdfq_example.mjs
```

For Livebook, open `examples/clients/livebook/pdfq.livemd` in Livebook.

Environment variables (Python and Node):

| Variable | Default | |
|---|---|---|
| `PDFQ_URL` | `http://localhost:3000` | Server URL |
| `PDFQ_WEBHOOK_SECRET` | `dev-secret` | Must match the server's; empty skips the webhook scenario |
| `PDFQ_CALLBACK_HOST` | `host.docker.internal` | Host the server uses to reach the receiver |
| `PDFQ_CALLBACK_PORT` | `8765` | Port of the webhook receiver |
