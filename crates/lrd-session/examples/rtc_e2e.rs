//! Teste de ponta a ponta do vídeo e dos comandos pelo WebRTC, contra um navegador real
//! (Chromium = WebView2 do Windows; WebKit = Safari/WKWebView do Mac).
//!
//! Usa o mesmo código do app: codificador (`lrd_video::h264`), envio com pacing/FEC
//! (`rtc` + `rtp_video`) e, no navegador, o `MediaEngine` da interface.
//!
//! ```text
//! cargo run --release --example rtc_e2e -- --port 8800 --page tools/e2e/page.html \
//!     --script tools/e2e/dist/media.js --seconds 12 [--hevc] [--no-fec]
//! ```
//! Imprime `LISTENING` quando pronto e `RESULT {json}` no fim.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lrd_session::rtc::{InputHandler, RtcCodecs, RtcSender};
use lrd_video::h264::H264Encoder;
use lrd_video::{CaptureCmd, PixelFormat, QualitySettings, RawFrame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const W: u32 = 1280;
const H: u32 = 720;
const FPS: u32 = 30;

struct Args {
    port: u16,
    page: String,
    script: String,
    seconds: u64,
    hevc: bool,
    no_fec: bool,
    lrd: bool,
    bitrate: u32,
}

fn args() -> Args {
    let mut a = Args {
        port: 8800,
        page: "tools/e2e/page.html".into(),
        script: "tools/e2e/dist/media.js".into(),
        seconds: 12,
        hevc: false,
        no_fec: false,
        lrd: false,
        bitrate: 6_000_000,
    };
    let v: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < v.len() {
        let next = || v.get(i + 1).cloned().unwrap_or_default();
        match v[i].as_str() {
            "--port" => a.port = next().parse().unwrap(),
            "--page" => a.page = next(),
            "--script" => a.script = next(),
            "--seconds" => a.seconds = next().parse().unwrap(),
            "--bitrate" => a.bitrate = next().parse().unwrap(),
            "--hevc" => {
                a.hevc = true;
                i += 1;
                continue;
            }
            "--lrd" => {
                a.lrd = true;
                i += 1;
                continue;
            }
            "--no-fec" => {
                a.no_fec = true;
                i += 1;
                continue;
            }
            other => panic!("argumento desconhecido: {other}"),
        }
        i += 2;
    }
    a
}

#[derive(Default)]
struct Counters {
    moves: AtomicU64,
    buttons: AtomicU64,
    keys: AtomicU64,
    scrolls: AtomicU64,
    pings: AtomicU64,
}

struct State {
    args: Args,
    rtc: tokio::sync::Mutex<Option<Arc<RtcSender>>>,
    counters: Arc<Counters>,
    encoder_info: Arc<Mutex<(String, bool, u64)>>,
    cap: Mutex<Option<std::sync::mpsc::Sender<CaptureCmd>>>,
    started: Mutex<Option<Instant>>,
}

/// Quadro sintético: fundo em degradê, barra que anda (movimento), xadrez fino (nitidez,
/// como texto) e um bloco que muda de cor a cada quadro.
fn synth(n: u64) -> RawFrame {
    let mut data = vec![0u8; (W * H * 4) as usize];
    let bar = ((n * 12) % W as u64) as u32;
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let (mut r, mut g, mut b) = ((x * 255 / W) as u8, (y * 255 / H) as u8, 96u8);
            if x >= bar && x < bar + 40 {
                (r, g, b) = (250, 250, 250);
            }
            // Região de nitidez: xadrez de 2 px em (100..356, 100..228).
            if (100..356).contains(&x) && (100..228).contains(&y) {
                let on = ((x / 2) + (y / 2)) % 2 == 0;
                let v = if on { 255 } else { 0 };
                (r, g, b) = (v, v, v);
            }
            if (1100..1180).contains(&x) && (600..680).contains(&y) {
                (r, g, b) = ((n * 37 % 255) as u8, (n * 91 % 255) as u8, (n * 13 % 255) as u8);
            }
            data[i] = b;
            data[i + 1] = g;
            data[i + 2] = r;
            data[i + 3] = 255;
        }
    }
    RawFrame::cpu(W, H, data, PixelFormat::Bgra, lrd_protocol::now_us())
}

fn input_handler(c: Arc<Counters>) -> InputHandler {
    Arc::new(move |b: &[u8]| {
        match b.first()? {
            1 => c.moves.fetch_add(1, Ordering::Relaxed),
            2 => c.buttons.fetch_add(1, Ordering::Relaxed),
            3 => c.keys.fetch_add(1, Ordering::Relaxed),
            4 => c.scrolls.fetch_add(1, Ordering::Relaxed),
            5 if b.len() >= 9 => {
                c.pings.fetch_add(1, Ordering::Relaxed);
                let mut out = vec![6];
                out.extend_from_slice(&b[1..9]);
                out.extend_from_slice(&lrd_protocol::now_us().to_le_bytes());
                return Some(out);
            }
            _ => 0,
        };
        None
    })
}

async fn start_rtc(st: &Arc<State>, browser_hevc: bool) -> anyhow::Result<String> {
    let (cap_tx, cap_rx) = std::sync::mpsc::channel::<CaptureCmd>();
    let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
    let codecs = RtcCodecs { hevc: st.args.hevc && browser_hevc, no_fec: st.args.no_fec, lrd: st.args.lrd };
    *st.cap.lock().unwrap() = Some(cap_tx.clone());
    let (rtc, offer) =
        RtcSender::start(cap_tx, st.args.bitrate, state_tx, input_handler(st.counters.clone()), codecs).await?;
    *st.rtc.lock().await = Some(rtc.clone());

    // Codificador numa thread (como no app), quadros para a tarefa de envio.
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
    let want_hevc = codecs.hevc;
    let lrd = codecs.lrd;
    let bitrate = st.args.bitrate;
    let info = st.encoder_info.clone();
    std::thread::spawn(move || {
        let q = QualitySettings { jpeg_quality: 80, max_width: W, max_fps: FPS, bitrate_bps: bitrate };
        if lrd {
            // codecLRD: nenhum quadro pode ser descartado depois de codificado (espera a fila).
            let mut enc = lrd_video::lrd::LrdEncoder::new();
            let mut n = 0u64;
            let interval = Duration::from_micros(1_000_000 / FPS as u64);
            let mut next = Instant::now();
            loop {
                while let Ok(cmd) = cap_rx.try_recv() {
                    if let CaptureCmd::Keyframe = cmd {
                        enc.force_keyframe();
                    }
                }
                let f = synth(n);
                let out = enc.encode(&f, q.jpeg_quality).or_else(|| enc.refine_due().then(|| enc.refine(lrd_protocol::now_us())).flatten());
                if let Some(bytes) = out {
                    if let Ok(mut i) = info.lock() {
                        let l = enc.last;
                        *i = (format!("codecLRD {}/{} blocos, {} cópias, {} B", l.dirty_tiles, l.total_tiles, l.copies, l.bytes), false, 0);
                    }
                    if frame_tx.blocking_send(bytes).is_err() {
                        return;
                    }
                }
                n += 1;
                next += interval;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
        }
        let mut enc = H264Encoder::new(&q).expect("codificador");
        enc.set_hevc(want_hevc);
        let mut n = 0u64;
        let interval = Duration::from_micros(1_000_000 / FPS as u64);
        let mut next = Instant::now();
        loop {
            while let Ok(cmd) = cap_rx.try_recv() {
                match cmd {
                    CaptureCmd::Keyframe => enc.force_keyframe(),
                    CaptureCmd::SetBitrate(b) => enc.set_bitrate(b),
                    CaptureCmd::SetHevc(h) => enc.set_hevc(h),
                    _ => {}
                }
            }
            let f = synth(n);
            if let Some(bytes) = enc.encode(&f) {
                if let Ok(mut i) = info.lock() {
                    *i = (enc.name().to_string(), enc.is_hevc(), enc.bitrate() as u64);
                }
                match frame_tx.try_send(bytes) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {} // descarta, como no app
                    Err(_) => return,
                }
            }
            n += 1;
            next += interval;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    });
    let rtc_send = rtc.clone();
    tokio::spawn(async move {
        while let Some(f) = frame_rx.recv().await {
            if lrd {
                rtc_send.write_media(&f).await;
            } else {
                rtc_send.write_video(&f).await;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(s) = state_rx.recv().await {
            println!("WEBRTC {s:?}");
        }
    });
    Ok(offer)
}

async fn respond(sock: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(body).await;
}

async fn handle(mut sock: TcpStream, st: Arc<State>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let (head_end, content_len) = loop {
        let n = match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            break (p + 4, len);
        }
    };
    while buf.len() < head_end + content_len {
        let n = match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    match (method, path.split('?').next().unwrap_or("/")) {
        ("GET", "/") => match std::fs::read(&st.args.page) {
            Ok(b) => respond(&mut sock, "200 OK", "text/html; charset=utf-8", &b).await,
            Err(e) => respond(&mut sock, "500 Error", "text/plain", e.to_string().as_bytes()).await,
        },
        ("GET", "/media.js") => match std::fs::read(&st.args.script) {
            Ok(b) => respond(&mut sock, "200 OK", "text/javascript", &b).await,
            Err(e) => respond(&mut sock, "500 Error", "text/plain", e.to_string().as_bytes()).await,
        },
        ("GET", "/config") => {
            let cfg = format!(
                r#"{{"seconds":{},"hevc":{},"fec":{},"lrd":{}}}"#,
                st.args.seconds, st.args.hevc, !st.args.no_fec, st.args.lrd
            );
            respond(&mut sock, "200 OK", "application/json", cfg.as_bytes()).await
        }
        ("GET", "/offer") => {
            let browser_hevc = path.contains("h265=1");
            match start_rtc(&st, browser_hevc).await {
                Ok(sdp) => respond(&mut sock, "200 OK", "application/sdp", sdp.as_bytes()).await,
                Err(e) => respond(&mut sock, "500 Error", "text/plain", format!("{e:#}").as_bytes()).await,
            }
        }
        ("POST", "/answer") => {
            let rtc = st.rtc.lock().await.clone();
            let r = match rtc {
                Some(r) => r.accept_answer(body).await.map_err(|e| format!("{e:#}")),
                None => Err("sem oferta".into()),
            };
            *st.started.lock().unwrap() = Some(Instant::now());
            match r {
                Ok(()) => respond(&mut sock, "200 OK", "text/plain", b"ok").await,
                Err(e) => respond(&mut sock, "500 Error", "text/plain", e.as_bytes()).await,
            }
        }
        ("GET", "/keyframe") => {
            if let Some(c) = st.cap.lock().unwrap().as_ref() {
                let _ = c.send(CaptureCmd::Keyframe);
            }
            println!("PAGE pediu quadro completo");
            respond(&mut sock, "200 OK", "text/plain", b"ok").await
        }
        ("POST", "/log") => {
            println!("PAGE {body}");
            respond(&mut sock, "200 OK", "text/plain", b"ok").await
        }
        ("POST", "/result") => {
            let c = &st.counters;
            let (enc_name, enc_hevc, enc_bitrate) = st.encoder_info.lock().unwrap().clone();
            let negotiated = match st.rtc.lock().await.clone() {
                Some(r) => r.negotiated().await,
                None => (false, false, false),
            };
            let host = format!(
                r#"{{"encoder":"{}","encoderHevc":{},"bitrateBps":{},"negH264":{},"negH265":{},"negFec":{},"moves":{},"buttons":{},"keys":{},"scrolls":{},"pings":{},"dropPct":"{}"}}"#,
                enc_name,
                enc_hevc,
                enc_bitrate,
                negotiated.0,
                negotiated.1,
                negotiated.2,
                c.moves.load(Ordering::Relaxed),
                c.buttons.load(Ordering::Relaxed),
                c.keys.load(Ordering::Relaxed),
                c.scrolls.load(Ordering::Relaxed),
                c.pings.load(Ordering::Relaxed),
                std::env::var("LRD_TEST_DROP_PCT").unwrap_or_default(),
            );
            println!("RESULT {{\"host\":{host},\"page\":{body}}}");
            respond(&mut sock, "200 OK", "text/plain", b"ok").await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::process::exit(0);
        }
        _ => respond(&mut sock, "404 Not Found", "text/plain", b"").await,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,webrtc_ice=debug")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = args();
    let listener = TcpListener::bind(("127.0.0.1", args.port)).await?;
    let limit = args.seconds + 60;
    let st = Arc::new(State {
        args,
        rtc: tokio::sync::Mutex::new(None),
        counters: Arc::new(Counters::default()),
        encoder_info: Arc::new(Mutex::new((String::new(), false, 0))),
        cap: Mutex::new(None),
        started: Mutex::new(None),
    });
    println!("LISTENING");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(limit)).await;
        println!("RESULT {{\"error\":\"tempo esgotado\"}}");
        std::process::exit(2);
    });
    loop {
        let (sock, _) = listener.accept().await?;
        tokio::spawn(handle(sock, st.clone()));
    }
}
