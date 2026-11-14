function gateError(code, message) {
  const error = new Error(code + ": " + message);
  error.code = code;
  return error;
}

export async function assertWorkbenchResponses(request, baseUrl) {
  async function get(path, label, unavailableCode) {
    let response;
    try {
      response = await request(new URL(path, baseUrl));
    } catch (_) {
      throw gateError(unavailableCode, label + " endpoint is unreachable");
    }
    if (!response || response.status !== 200) {
      throw gateError("BAD_STATUS", label + " endpoint did not return HTTP 200");
    }
    return response;
  }

  const page = await get("/", "Fleet page", "NO_LISTENER");
  const html = await page.text();
  if (!/<title>[^<]*Fleet[^<]*<\/title>/i.test(html) ||
      !html.includes('id="heading"') ||
      !html.includes('src="/workbench.js"')) {
    throw gateError("MARKER_MISSING", "Fleet page response is not the expected workbench document");
  }

  const script = await get("/workbench.js", "Fleet workbench script", "SCRIPT_ERROR");
  const source = await script.text();
  if (!source.includes("setupUi();") || !source.includes('id="setup-again"')) {
    throw gateError("SCRIPT_ERROR", "Fleet workbench script does not initialize the expected control");
  }

  return {
    pageStatus: page.status,
    scriptStatus: script.status,
    scriptBytes: Buffer.byteLength(source, "utf8")
  };
}
