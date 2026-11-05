// Roda o teste de ponta a ponta em navegadores reais e confere os resultados.
//   node tools/e2e/run.mjs <binário rtc_e2e> [chromium,webkit]
import { spawn } from "node:child_process";
import { chromium, webkit } from "playwright";

const bin = process.argv[2];
const wanted = (process.argv[3] ?? "chromium,webkit").split(",");
const SECONDS = 12;

const configs = [
  { name: "H.264", args: [] },
  { name: "H.264, 3% de perda", args: [], env: { LRD_TEST_DROP_PCT: "3" }, lossy: true },
  { name: "H.265 (se suportado)", args: ["--hevc"] },
  { name: "H.264 + FEC (experimental)", args: [], env: { LRD_FEC: "1" }, informative: true },
];

function runHost(port, cfg) {
  return new Promise((resolve, reject) => {
    const p = spawn(bin, ["--port", String(port), "--seconds", String(SECONDS), ...cfg.args], {
      env: { ...process.env, RUST_LOG: "info", LRD_TEST_LOOPBACK: "1", ...(cfg.env ?? {}) },
      stdio: ["ignore", "pipe", "inherit"],
    });
    let out = "";
    let ready;
    const readyP = new Promise((r) => (ready = r));
    p.stdout.on("data", (d) => {
      const s = d.toString();
      out += s;
      for (const line of s.split("\n")) {
        if (line.startsWith("LISTENING")) ready();
        else if (line.startsWith("PAGE ") || line.startsWith("WEBRTC ")) console.log("   ", line);
      }
    });
    p.on("exit", () => {
      const m = out.match(/^RESULT (.*)$/m);
      if (m) resolve({ result: JSON.parse(m[1]) });
      else reject(new Error("host terminou sem resultado:\n" + out));
    });
    runHost.last = { p, readyP };
  });
}

// Navegador com a mesma interface mínima: newPage() → { goto, close }.
const launchers = {
  chromium: () =>
    chromium.launch({ args: ["--autoplay-policy=no-user-gesture-required", "--use-fake-ui-for-media-stream"] }),
  webkit: () => webkit.launch(),
  // Safari de verdade (mesmo motor do WKWebView do app no Mac), via safaridriver.
  safari: async () => {
    const { Builder } = await import("selenium-webdriver");
    const driver = await new Builder().forBrowser("safari").build();
    return {
      newPage: async () => ({
        on: () => {},
        goto: (url) => driver.get(url),
        close: async () => {
          try {
            await driver.get("about:blank");
          } catch {}
        },
      }),
      close: () => driver.quit(),
    };
  },
};

const rows = [];
let failures = 0;
let port = 8800;
for (const bname of wanted) {
  let browser;
  try {
    browser = await launchers[bname]();
  } catch (e) {
    console.log(`!! ${bname} indisponível: ${e.message}`);
    continue;
  }
  for (const cfg of configs) {
    port++;
    console.log(`\n== ${bname} · ${cfg.name}`);
    const done = runHost(port, cfg);
    await runHost.last.readyP;
    const page = await browser.newPage();
    page.on("console", (m) => console.log("    console:", m.text()));
    page.on("pageerror", (e) => console.log("    pageerror:", e.message));
    page.goto(`http://127.0.0.1:${port}/`).catch((e) => console.log("    goto:", e.message));
    let res;
    try {
      res = (await done).result;
    } catch (e) {
      console.log(e.message);
      failures++;
      await page.close();
      continue;
    }
    await page.close();
    if (res.error) {
      console.log("   ERRO:", res.error);
      failures++;
      continue;
    }
    const { host, page: pg } = res;
    console.log("   host:", JSON.stringify(host));
    console.log("   page:", JSON.stringify(pg));
    const hevcCase = cfg.args.includes("--hevc");
    if (!pg.directInput && pg.frames === 0) {
      console.log("   (conexão WebRTC não se formou neste navegador)");
    }
    if (hevcCase && !host.negH265) {
      console.log("   (H.265 não negociado neste navegador/máquina — caso ignorado)");
      rows.push([bname, cfg.name, "ignorado", "", "", "", "", "", ""]);
      continue;
    }
    const problems = [];
    // Os Macs do GitHub são máquinas virtuais (codificador lento, ~16 quadros/s).
    if (!(pg.fps >= (process.platform === "darwin" ? 12 : 20))) problems.push(`fps ${pg.fps?.toFixed(1)}`);
    if (pg.freezes > (cfg.lossy ? 4 : 1)) problems.push(`${pg.freezes} travadas`);
    if (!cfg.lossy && host.bitrateBps < 3_000_000) problems.push(`taxa caiu para ${(host.bitrateBps / 1e6).toFixed(1)} Mbps`);
    if (pg.width !== 1280) problems.push(`largura ${pg.width}`);
    if (!pg.directInput) problems.push("comandos fora do canal direto");
    if (host.moves < pg.sentDirect * 0.8) problems.push(`movimentos ${host.moves}/${pg.sentDirect}`);
    if (host.buttons < pg.clicks * 2 - 4) problems.push(`cliques ${host.buttons}/${pg.clicks * 2}`);
    if (host.keys < pg.clicks * 2 - 4) problems.push(`teclas ${host.keys}/${pg.clicks * 2}`);
    if (pg.glassMs != null && pg.glassMs > 200) problems.push(`atraso ${pg.glassMs.toFixed(0)} ms`);
    if (pg.sharpness != null && pg.sharpness < 60) problems.push(`nitidez ${pg.sharpness.toFixed(0)}`);
    rows.push([
      bname,
      cfg.name,
      problems.length ? (cfg.informative ? "falhou (informativo)" : "FALHOU") : "ok",
      pg.codec,
      pg.fps?.toFixed(1),
      pg.freezes,
      pg.glassMs?.toFixed(0) ?? "-",
      pg.inputRttMs?.toFixed(1) ?? "-",
      pg.sharpness?.toFixed(0) ?? "-",
      problems.join("; "),
    ]);
    if (problems.length && !cfg.informative) failures++;
  }
  await browser.close();
}

console.log("\nnavegador | caso | resultado | codec | fps | travadas | atraso ms | comando ida/volta ms | nitidez | problemas");
for (const r of rows) console.log(r.join(" | "));
process.exit(failures ? 1 : 0);
