// PDFQ client example: sync compile, sync upload, async job with webhook.
//
// Node 18+, no dependencies. Run from the repository root:
//
//     node examples/clients/node/pdfq_example.mjs
//
// PDFs are written to the current directory.

import { createHmac, timingSafeEqual } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";

const BASE_URL = (process.env.PDFQ_URL ?? "http://localhost:3000").replace(/\/$/, "");
const SECRET = process.env.PDFQ_WEBHOOK_SECRET ?? "dev-secret";
// Host the PDFQ container uses to reach this script.
const CALLBACK_HOST = process.env.PDFQ_CALLBACK_HOST ?? "host.docker.internal";
const CALLBACK_PORT = Number(process.env.PDFQ_CALLBACK_PORT ?? 8765);
const INVOICE_DATA = new URL("../../invoice/data.json", import.meta.url);

// Print a PDFQ error body: {"error": {"kind", "message", "diagnostics"}}.
async function printError(resp) {
  const { error } = await resp.json();
  console.log(`  HTTP ${resp.status} ${error.kind}: ${error.message}`);
  for (const d of error.diagnostics ?? []) {
    console.log(`    ${d.file}:${d.line}:${d.column} ${d.message}`);
  }
}

function save(name, pdf) {
  writeFileSync(name, pdf);
  console.log(`  wrote ${name} (${pdf.length} bytes, starts with ${pdf.subarray(0, 4)})`);
}

// POST /compile with a JSON body: mounted template + data.
async function syncTemplate() {
  console.log("sync: template 'invoice'");
  const data = JSON.parse(readFileSync(INVOICE_DATA, "utf8"));
  const resp = await fetch(`${BASE_URL}/compile`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ template: "invoice", data }),
  });
  if (!resp.ok) return printError(resp);
  save("invoice.pdf", Buffer.from(await resp.arrayBuffer()));
  console.log(`  pages: ${resp.headers.get("x-pdfq-pages")}`);
}

// POST /compile multipart: the filename is the path in the project.
async function syncUpload() {
  console.log("sync: uploaded source");
  const form = new FormData();
  form.append("file", new Blob(['= Hello #sys.inputs.at("who", default: "world")!\n']), "main.typ");
  form.append("input.who", "PDFQ");
  const resp = await fetch(`${BASE_URL}/compile`, { method: "POST", body: form });
  if (!resp.ok) return printError(resp);
  save("hello.pdf", Buffer.from(await resp.arrayBuffer()));
}

// Check x-pdfq-signature: sha256=hex(HMAC-SHA256(secret, "<timestamp>.<body>")).
function verify(secret, headers, body, maxAge = 300) {
  const ts = headers["x-pdfq-timestamp"] ?? "";
  const signature = Buffer.from(headers["x-pdfq-signature"] ?? "");
  if (!/^\d+$/.test(ts) || Math.abs(Date.now() / 1000 - Number(ts)) > maxAge) {
    return false; // missing or stale timestamp: possible replay
  }
  const mac = createHmac("sha256", secret).update(`${ts}.`).update(body).digest("hex");
  const expected = Buffer.from(`sha256=${mac}`);
  // timingSafeEqual throws on different lengths.
  return signature.length === expected.length && timingSafeEqual(signature, expected);
}

// Resolve with the first webhook delivery received on CALLBACK_PORT.
function receiveWebhook() {
  let server;
  const delivery = new Promise((resolve) => {
    server = createServer((req, res) => {
      const chunks = [];
      req.on("data", (c) => chunks.push(c));
      req.on("end", () => {
        const body = Buffer.concat(chunks);
        const ok = verify(SECRET, req.headers, body);
        // Any 2xx is a delivery, 401 is final (no retry).
        res.writeHead(ok ? 204 : 401).end();
        resolve({ ok, headers: req.headers, body });
      });
    });
  });
  return new Promise((ready) =>
    server.listen(CALLBACK_PORT, "0.0.0.0", () => ready({ server, delivery })),
  );
}

// POST /jobs with callback_url, then wait for the signed webhook.
async function asyncWebhook() {
  console.log("async: job with webhook");
  const { server, delivery } = await receiveWebhook();
  try {
    const callback = `http://${CALLBACK_HOST}:${CALLBACK_PORT}/hook`;
    const form = new FormData();
    form.append("file", new Blob(["= Async job\nDelivered by webhook.\n"]), "main.typ");
    form.append("callback_url", callback);
    const resp = await fetch(`${BASE_URL}/jobs`, { method: "POST", body: form });
    if (resp.status !== 202) return printError(resp);
    const { job_id } = await resp.json();
    console.log(`  queued ${job_id}, waiting for ${callback}`);
    const { ok, headers, body } = await delivery;
    if (!ok) {
      console.log("  signature REJECTED");
    } else if (headers["x-pdfq-status"] === "succeeded") {
      console.log("  signature ok");
      save("async.pdf", body);
    } else {
      console.log("  signature ok, job failed:", JSON.parse(body).error);
    }
  } finally {
    server.close();
  }
}

// POST /jobs without callback_url, poll GET /jobs/{id}, fetch the PDF.
async function asyncPolling() {
  console.log("async: job with polling");
  const form = new FormData();
  form.append("file", new Blob(["= Async job\nFetched by polling.\n"]), "main.typ");
  const resp = await fetch(`${BASE_URL}/jobs`, { method: "POST", body: form });
  if (resp.status !== 202) return printError(resp);
  const statusUrl = BASE_URL + (await resp.json()).status_url;
  let job;
  while (["queued", "running"].includes((job = await (await fetch(statusUrl)).json()).status)) {
    await new Promise((r) => setTimeout(r, 500));
  }
  console.log(`  status: ${job.status}`);
  const pdf = await fetch(`${statusUrl}/pdf`);
  if (!pdf.ok) return printError(pdf);
  save("polled.pdf", Buffer.from(await pdf.arrayBuffer()));
}

await syncTemplate();
await syncUpload();
// Webhooks need PDFQ_WEBHOOK_SECRET on both sides; poll otherwise.
if (SECRET) await asyncWebhook();
await asyncPolling();
