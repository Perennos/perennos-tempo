import assert from "node:assert/strict";
import test from "node:test";
import { assertWorkbenchResponses } from "./browser_gate.mjs";

const baseUrl = "http://127.0.0.1:18776/";
const pageHtml = '<!doctype html><html><head><title>Perennos Tempo — Fleet</title></head><body><h1 id="heading">Your fleet</h1><script src="/workbench.js" defer></script></body></html>';
const workbenchJs = 'function setupUi() { const settings = document.createElement("section"); settings.innerHTML = \'<button id="setup-again">Run setup again</button>\'; } setupUi();';

function server(page = pageHtml, script = workbenchJs, statuses = {}) {
  return async url => {
    const route = new URL(url).pathname;
    if (route === "/") return { status: statuses.page ?? 200, text: async () => page };
    if (route === "/workbench.js") return { status: statuses.script ?? 200, text: async () => script };
    return { status: 404, text: async () => "" };
  };
}

test("accepts the actual Fleet page and workbench script contract", async () => {
  const result = await assertWorkbenchResponses(server(), baseUrl);
  assert.deepEqual(result, { pageStatus: 200, scriptStatus: 200, scriptBytes: Buffer.byteLength(workbenchJs) });
});

test("reports NO_LISTENER for the browser error-page condition", async () => {
  await assert.rejects(
    () => assertWorkbenchResponses(async () => { throw new Error("connection refused"); }, baseUrl),
    error => error.code === "NO_LISTENER"
  );
});

test("reports MARKER_MISSING for an unrelated completed HTML document", async () => {
  await assert.rejects(
    () => assertWorkbenchResponses(server("<html><title>127.0.0.1</title></html>"), baseUrl),
    error => error.code === "MARKER_MISSING"
  );
});

test("reports SCRIPT_ERROR when the page script or first control is absent", async () => {
  await assert.rejects(
    () => assertWorkbenchResponses(server(pageHtml, "console.log('not the app');"), baseUrl),
    error => error.code === "SCRIPT_ERROR"
  );
});

test("reports BAD_STATUS for non-200 page and script responses", async () => {
  await assert.rejects(
    () => assertWorkbenchResponses(server(pageHtml, workbenchJs, { page: 404 }), baseUrl),
    error => error.code === "BAD_STATUS"
  );
  await assert.rejects(
    () => assertWorkbenchResponses(server(pageHtml, workbenchJs, { script: 404 }), baseUrl),
    error => error.code === "BAD_STATUS"
  );
});
