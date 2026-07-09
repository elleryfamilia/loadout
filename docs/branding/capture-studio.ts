// Capture the studio Loadouts tab with the "rust" loadout selected.
// Usage: deno run --allow-net --allow-run --allow-write capture-studio.ts <bootstrapUrl> <outPng>
const [bootstrapUrl, outPng] = Deno.args;
const DEBUG_PORT = 9223;

const chrome = new Deno.Command(
  "/Applications/Chromium.app/Contents/MacOS/Chromium",
  {
    args: [
      "--headless=new",
      "--disable-gpu",
      `--remote-debugging-port=${DEBUG_PORT}`,
      "--no-first-run",
      "about:blank",
    ],
    stdout: "null",
    stderr: "null",
  },
).spawn();

async function jsonList(): Promise<any[]> {
  for (let i = 0; i < 50; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${DEBUG_PORT}/json/list`);
      return await r.json();
    } catch {
      await new Promise((r) => setTimeout(r, 200));
    }
  }
  throw new Error("chrome debug port never came up");
}

const targets = await jsonList();
const page = targets.find((t) => t.type === "page");
const ws = new WebSocket(page.webSocketDebuggerUrl);

let msgId = 0;
const pending = new Map<number, (v: any) => void>();
const events: ((m: any) => void)[] = [];

ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) {
    pending.get(m.id)!(m);
    pending.delete(m.id);
  } else if (m.method) {
    events.forEach((f) => f(m));
  }
};

function send(method: string, params: any = {}): Promise<any> {
  const id = ++msgId;
  return new Promise((resolve) => {
    pending.set(id, resolve);
    ws.send(JSON.stringify({ id, method, params }));
  });
}

await new Promise((r) => (ws.onopen = r));

await send("Page.enable");
await send("Runtime.enable");
await send("Emulation.setDeviceMetricsOverride", {
  width: 1180,
  height: 956,
  deviceScaleFactor: 2,
  mobile: false,
});

const loaded = new Promise<void>((resolve) => {
  events.push((m) => m.method === "Page.loadEventFired" && resolve());
});
await send("Page.navigate", { url: bootstrapUrl });
await loaded;
await new Promise((r) => setTimeout(r, 1200)); // let htmx/assets settle

// Click the rust loadout card (htmx GET /nrust/select).
const click = await send("Runtime.evaluate", {
  expression: `
    (() => {
      const els = [...document.querySelectorAll('[hx-get]')];
      const attrs = els.map(e => e.getAttribute('hx-get'));
      const rust = els.find(e => e.getAttribute('hx-get') === '/profiles/rust/select');
      if (!rust) return "NOT FOUND. hx-gets: " + JSON.stringify(attrs.slice(0, 40));
      rust.click();
      return "clicked " + rust.getAttribute('hx-get');
    })()`,
  returnByValue: true,
});
console.log("click:", JSON.stringify(click.result));
await new Promise((r) => setTimeout(r, 1500)); // htmx swap

const shot = await send("Page.captureScreenshot", { format: "png" });
await Deno.writeFile(
  outPng,
  Uint8Array.from(atob(shot.result.data), (c) => c.charCodeAt(0)),
);
console.log("saved", outPng);

ws.close();
chrome.kill();
