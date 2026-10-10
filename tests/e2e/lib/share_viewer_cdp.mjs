// Load an exported share bundle's viewer in headless Chromium over the DevTools
// protocol and report what a reader actually gets (br-kp1in.42).
//
// Usage: node share_viewer_cdp.mjs VIEWER_URL USER_DATA_DIR [CHROMIUM]
// Prints one JSON report on stdout:
//   state          controller state once loaded ({loading, messages})
//   alpine_errors  count of "Alpine Expression Error" console warnings
//   exceptions     uncaught page exceptions
//   dialogs        alert/confirm texts (the viewer alerts on a failed init)
//   rendered       [{id, subject, body_html}] after selecting each message
//   sanitized      renderMarkdownSafe() output for fixed hostile inputs
//   pwned          DOM markers a hostile payload sets if it ever executes
//   live_handlers  on* attributes or javascript: URLs left in rendered bodies
// Uses Node's built-in WebSocket and fetch (Node >= 22); no npm packages.
import { spawn } from "node:child_process";
import { existsSync, readFileSync, statSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const [url, userDataDir, chromium = "chromium-browser"] = process.argv.slice(2);
// A reused profile keeps the DevToolsActivePort of a previous (killed)
// browser; only a file written by this launch names our port.
const launchedAtMs = Date.now() - 1000;
const browser = spawn(chromium, [
  "--headless=new", "--no-sandbox", "--disable-gpu", "--no-first-run",
  "--remote-debugging-port=0", `--user-data-dir=${userDataDir}`, "about:blank",
], { stdio: ["ignore", "ignore", "ignore"] });

async function devtoolsPort() {
  const file = `${userDataDir}/DevToolsActivePort`;
  for (let i = 0; i < 300; i += 1) {
    if (existsSync(file) && statSync(file).mtimeMs >= launchedAtMs) {
      const port = readFileSync(file, "utf8").split("\n")[0].trim();
      if (port) return port;
    }
    await sleep(100);
  }
  throw new Error(`no DevToolsActivePort under ${userDataDir} (is the profile dir writable by ${chromium}?)`);
}

const report = { url, console: [], exceptions: [], dialogs: [] };
try {
  const port = await devtoolsPort();
  const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  const ws = new WebSocket(targets.find((t) => t.type === "page").webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  let nextId = 0;
  const pending = new Map();
  const send = (method, params = {}) => new Promise((resolve) => {
    nextId += 1;
    pending.set(nextId, resolve);
    ws.send(JSON.stringify({ id: nextId, method, params }));
  });
  ws.onmessage = (event) => {
    const msg = JSON.parse(event.data);
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    } else if (msg.method === "Runtime.consoleAPICalled") {
      const text = msg.params.args.map((a) => a.value ?? a.description ?? "").join(" ");
      report.console.push(`${msg.params.type}: ${text}`.slice(0, 400));
    } else if (msg.method === "Runtime.exceptionThrown") {
      const d = msg.params.exceptionDetails;
      report.exceptions.push(`${d.text} ${d.exception?.description ?? ""}`.slice(0, 400));
    } else if (msg.method === "Page.javascriptDialogOpening") {
      // An open dialog blocks the page; record it and dismiss it.
      report.dialogs.push(`${msg.params.type}: ${msg.params.message}`.slice(0, 400));
      send("Page.handleJavaScriptDialog", { accept: true });
    }
  };
  const evaluate = async (expression, label) => {
    const res = await Promise.race([
      send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }),
      sleep(30000).then(() => ({ timedOut: true })),
    ]);
    if (res.timedOut) return { error: `${label}: no answer in 30 s` };
    const d = res.result?.exceptionDetails;
    if (d) return { error: `${label}: ${d.text} ${d.exception?.description ?? ""}`.slice(0, 400) };
    return res.result?.result?.value;
  };

  await send("Runtime.enable");
  await send("Page.enable");
  await send("Page.navigate", { url });
  for (let i = 0; i < 150; i += 1) {
    await sleep(200);
    report.state = await evaluate(`(() => {
      const root = document.querySelector('[x-data]');
      const data = root && window.Alpine ? window.Alpine.$data(root) : null;
      return data ? { loading: data.isLoading, messages: (data.allMessages || []).length } : null;
    })()`, "state");
    if (report.state && report.state.loading === false) break;
  }
  report.alpine_errors = report.console.filter((l) => l.includes("Alpine Expression Error")).length;
  report.rendered = await evaluate(`(async () => {
    const data = window.Alpine.$data(document.querySelector('[x-data]'));
    const out = [];
    for (const message of data.allMessages || []) {
      // The viewer selects the first message on load, and clicking the
      // selected message deselects it.
      if (data.selectedMessage?.id !== message.id) {
        await Promise.race([data.handleMessageClick(message), new Promise((r) => setTimeout(r, 5000))]);
      }
      let body = '';
      for (let i = 0; i < 40 && !body; i += 1) {
        await new Promise((r) => setTimeout(r, 50));
        body = [...document.querySelectorAll('[x-html="renderMarkdown(selectedMessage.body_md)"]')]
          .map((el) => el.innerHTML).find((html) => html.trim()) || '';
      }
      out.push({ id: message.id, subject: message.subject, body_html: body });
    }
    return out;
  })()`, "rendered");
  report.sanitized = await evaluate(`[
    "[click](javascript:document.documentElement.dataset.pwnedDirect='link')",
    "<img src=x onerror=\\"document.documentElement.dataset.pwnedDirect='img'\\">",
    "<a href=\\"https://example.invalid/ok\\">ok</a>",
  ].map((md) => String(renderMarkdownSafe(md)))`, "sanitized");
  report.pwned = await evaluate(`Object.keys(document.documentElement.dataset)
    .filter((key) => key.startsWith('pwned'))
    .map((key) => key + '=' + document.documentElement.dataset[key])`, "pwned");
  report.live_handlers = await evaluate(`[...document.querySelectorAll('[x-html] *')]
    .flatMap((el) => [...el.attributes]
      .filter((a) => /^on/i.test(a.name) || /^\\s*javascript:/i.test(a.value))
      .map((a) => el.tagName + ' ' + a.name))`, "live_handlers");
  ws.close();
} catch (error) {
  report.error = String(error);
} finally {
  browser.kill("SIGKILL");
}
console.log(JSON.stringify(report, null, 2));
