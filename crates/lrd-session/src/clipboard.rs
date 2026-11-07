//! Área de transferência compartilhada (texto) durante a sessão.
//!
//! Cada lado roda um vigia que lê a área de transferência a cada 400 ms. Texto novo copiado
//! aqui vai para o outro computador pelo canal de controle (QUIC + TLS); texto que chega do
//! outro lado é gravado aqui e lembrado, para não voltar em eco.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

/// Maior texto sincronizado (cabe com folga numa mensagem de controle de 256 KB).
pub const MAX_CLIPBOARD_BYTES: usize = 200 * 1024;
const POLL: Duration = Duration::from_millis(400);

/// Acesso à área de transferência do sistema (substituível nos testes).
pub trait ClipboardBackend: Send + 'static {
    fn get_text(&mut self) -> Option<String>;
    fn set_text(&mut self, text: &str) -> bool;
}

/// Vigia ativo; para sozinho quando é descartado.
pub struct ClipboardSync {
    stop: Arc<AtomicBool>,
    to_set: Sender<String>,
}

impl ClipboardSync {
    /// Inicia o vigia com a área de transferência do sistema. `on_local` recebe cada texto novo
    /// copiado neste computador. `None` se a área de transferência não estiver disponível.
    pub fn start(on_local: impl FnMut(String) + Send + 'static) -> Option<ClipboardSync> {
        let backend = SystemClipboard::open()?;
        Some(Self::start_with(backend, on_local, POLL))
    }

    pub fn start_with(
        backend: impl ClipboardBackend,
        on_local: impl FnMut(String) + Send + 'static,
        poll: Duration,
    ) -> ClipboardSync {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<String>();
        let stop_t = stop.clone();
        std::thread::Builder::new()
            .name("lrd-clipboard".into())
            .spawn(move || run(backend, on_local, rx, stop_t, poll))
            .ok();
        ClipboardSync { stop, to_set: tx }
    }

    /// Texto que chegou do outro computador: grava aqui (sem reenviar de volta).
    pub fn apply_remote(&self, text: String) {
        if text.len() <= MAX_CLIPBOARD_BYTES {
            let _ = self.to_set.send(text);
        }
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run(
    mut cb: impl ClipboardBackend,
    mut on_local: impl FnMut(String),
    rx: Receiver<String>,
    stop: Arc<AtomicBool>,
    poll: Duration,
) {
    // O que já estava copiado antes da sessão não é enviado; só o que for copiado depois.
    let mut last = cb.get_text();
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(poll) {
            Ok(text) => {
                if last.as_deref() != Some(text.as_str()) && cb.set_text(&text) {
                    last = Some(text);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(now) = cb.get_text() {
            if last.as_deref() != Some(now.as_str()) {
                last = Some(now.clone());
                if !now.is_empty() && now.len() <= MAX_CLIPBOARD_BYTES {
                    on_local(now);
                }
            }
        }
    }
}

struct SystemClipboard(arboard::Clipboard);

impl SystemClipboard {
    fn open() -> Option<Self> {
        match arboard::Clipboard::new() {
            Ok(c) => Some(SystemClipboard(c)),
            Err(e) => {
                tracing::warn!("área de transferência indisponível: {e}");
                None
            }
        }
    }
}

impl ClipboardBackend for SystemClipboard {
    fn get_text(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }
    fn set_text(&mut self, text: &str) -> bool {
        match self.0.set_text(text.to_owned()) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("não foi possível gravar na área de transferência: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Option<String>>>);
    impl ClipboardBackend for Fake {
        fn get_text(&mut self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        fn set_text(&mut self, text: &str) -> bool {
            *self.0.lock().unwrap() = Some(text.to_owned());
            true
        }
    }

    fn wait() {
        std::thread::sleep(Duration::from_millis(60));
    }

    #[test]
    fn sends_new_local_copies_but_not_preexisting_or_echo() {
        let fake = Fake::default();
        *fake.0.lock().unwrap() = Some("antes da sessão".into());
        let sent = Arc::new(Mutex::new(Vec::<String>::new()));
        let s2 = sent.clone();
        let sync = ClipboardSync::start_with(fake.clone(), move |t| s2.lock().unwrap().push(t), Duration::from_millis(10));
        wait();
        assert!(sent.lock().unwrap().is_empty(), "não deve enviar o que já estava copiado");

        *fake.0.lock().unwrap() = Some("olá do Mac".into());
        wait();
        assert_eq!(*sent.lock().unwrap(), vec!["olá do Mac".to_string()]);

        // Chega texto do outro lado: grava aqui e não devolve.
        sync.apply_remote("olá do Windows".into());
        wait();
        assert_eq!(fake.0.lock().unwrap().as_deref(), Some("olá do Windows"));
        assert_eq!(sent.lock().unwrap().len(), 1, "eco do texto recebido");

        // Copiar de novo o mesmo texto que veio do outro lado não reenvia; texto novo sim.
        *fake.0.lock().unwrap() = Some("outro".into());
        wait();
        assert_eq!(sent.lock().unwrap().last().map(String::as_str), Some("outro"));
        drop(sync);
    }

    #[test]
    fn ignores_huge_text() {
        let fake = Fake::default();
        let sent = Arc::new(Mutex::new(0usize));
        let s2 = sent.clone();
        let _sync = ClipboardSync::start_with(fake.clone(), move |_| *s2.lock().unwrap() += 1, Duration::from_millis(10));
        wait();
        *fake.0.lock().unwrap() = Some("x".repeat(MAX_CLIPBOARD_BYTES + 1));
        wait();
        assert_eq!(*sent.lock().unwrap(), 0);
    }
}
