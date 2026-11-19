//! Windows: mouse e teclado por um ajudante rodando como SISTEMA na sessão do usuário.
//!
//! Os pedidos do UAC (`consent.exe`) rodam com integridade "Sistema": nem um processo de
//! administrador consegue mandar cliques e teclas para eles (UIPI). O ajudante (aberto pelo
//! agente do sistema do acesso total) roda como SISTEMA na sessão do usuário, acompanha a área
//! de trabalho ativa e injeta o que o LRD mandar por um pipe local.
//!
//! O pipe só aceita administradores ELEVADOS e o SISTEMA (um processo comum do usuário não
//! consegue usá-lo para aprovar o próprio UAC).

use std::io::Write;
use std::time::{Duration, Instant};

use crate::InjectCmd;

/// Versão no nome: um ajudante antigo não conversa com um LRD novo de formato diferente.
pub const PIPE: &str = r"\\.\pipe\lrd-elevated-input-v1";
const MSG: usize = 13;

fn encode(cmd: &InjectCmd) -> [u8; MSG] {
    let (k, a, b, c): (u8, i32, i32, i32) = match *cmd {
        InjectCmd::MoveAbs { x, y } => (1, x, y, 0),
        InjectCmd::Button { button, pressed } => (2, button as i32, pressed as i32, 0),
        InjectCmd::Scroll { dx, dy } => (3, dx, dy, 0),
        InjectCmd::Key { hid, pressed } => (4, hid as i32, pressed as i32, 0),
        InjectCmd::ReleaseAll => (5, 0, 0, 0),
    };
    let mut m = [0u8; MSG];
    m[0] = k;
    m[1..5].copy_from_slice(&a.to_le_bytes());
    m[5..9].copy_from_slice(&b.to_le_bytes());
    m[9..13].copy_from_slice(&c.to_le_bytes());
    m
}

fn decode(m: &[u8; MSG]) -> Option<InjectCmd> {
    let i = |o: usize| i32::from_le_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]]);
    let (a, b) = (i(1), i(5));
    Some(match m[0] {
        1 => InjectCmd::MoveAbs { x: a, y: b },
        2 => InjectCmd::Button { button: a.clamp(0, 2) as u32, pressed: b != 0 },
        3 => InjectCmd::Scroll { dx: a, dy: b },
        4 => InjectCmd::Key { hid: a as u32, pressed: b != 0 },
        5 => InjectCmd::ReleaseAll,
        _ => return None,
    })
}

/// Lado do LRD: manda os comandos para o ajudante quando ele existe (acesso total ligado).
pub(crate) struct Forwarder {
    pipe: Option<std::fs::File>,
    last_try: Option<Instant>,
}

impl Forwarder {
    pub(crate) fn new() -> Self {
        Forwarder { pipe: None, last_try: None }
    }

    /// `true` se o ajudante recebeu o comando (não injetar localmente).
    pub(crate) fn send(&mut self, cmd: &InjectCmd) -> bool {
        if self.pipe.is_none() {
            if self.last_try.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                return false;
            }
            self.last_try = Some(Instant::now());
            match std::fs::OpenOptions::new().write(true).open(PIPE) {
                Ok(f) => {
                    tracing::info!("entrada pelo ajudante do sistema (pedidos do administrador controláveis)");
                    self.pipe = Some(f);
                }
                Err(_) => return false,
            }
        }
        let ok = self.pipe.as_mut().is_some_and(|p| p.write_all(&encode(cmd)).is_ok());
        if !ok {
            self.pipe = None;
        }
        ok
    }
}

/// Faz a thread atual usar a área de trabalho que está recebendo entrada agora (a normal, a
/// do UAC/tela segura ou a de bloqueio). Só funciona como SISTEMA.
pub(crate) struct DesktopFollower {
    current: windows_sys::Win32::System::StationsAndDesktops::HDESK,
    last: Option<Instant>,
}

impl DesktopFollower {
    pub(crate) fn new() -> Self {
        DesktopFollower { current: std::ptr::null_mut(), last: None }
    }

    pub(crate) fn sync(&mut self) {
        use windows_sys::Win32::Foundation::GENERIC_ALL;
        use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop, SetThreadDesktop};
        if self.last.is_some_and(|t| t.elapsed() < Duration::from_millis(150)) {
            return;
        }
        self.last = Some(Instant::now());
        unsafe {
            let h = OpenInputDesktop(0, 0, GENERIC_ALL);
            if h.is_null() {
                return;
            }
            if SetThreadDesktop(h) != 0 {
                if !self.current.is_null() {
                    CloseDesktop(self.current);
                }
                self.current = h;
            } else {
                CloseDesktop(h);
            }
        }
    }
}

/// Lado do ajudante (SISTEMA na sessão do usuário): atende o pipe e injeta. Não retorna.
pub fn serve() -> ! {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, PIPE_ACCESS_INBOUND};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    let injector = match crate::InputInjector::start_following_desktop() {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("ajudante: injeção indisponível: {e}");
            std::process::exit(1);
        }
    };
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let name = wide(PIPE);
    // Acesso: SISTEMA e Administradores; nível de integridade mínimo "alto" (só elevados).
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NW;;;HI)");
    loop {
        unsafe {
            let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
                tracing::error!("ajudante: descritor de segurança inválido");
                std::process::exit(1);
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let pipe = CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_INBOUND, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, 1, 0, 4096, 0, &sa);
            LocalFree(sd as _);
            if pipe == INVALID_HANDLE_VALUE {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            let connected = ConnectNamedPipe(pipe, std::ptr::null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED;
            if connected {
                let mut buf = [0u8; MSG];
                loop {
                    // Lê exatamente uma mensagem.
                    let mut got = 0usize;
                    while got < MSG {
                        let mut n = 0u32;
                        if ReadFile(pipe, buf[got..].as_mut_ptr(), (MSG - got) as u32, &mut n, std::ptr::null_mut()) == 0 || n == 0 {
                            got = usize::MAX;
                            break;
                        }
                        got += n as usize;
                    }
                    if got != MSG {
                        break;
                    }
                    if let Some(cmd) = decode(&buf) {
                        injector.send(cmd);
                    }
                }
                // O LRD desconectou: solta o que ficou pressionado.
                injector.send(InjectCmd::ReleaseAll);
                DisconnectNamedPipe(pipe);
            }
            CloseHandle(pipe);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for c in [
            InjectCmd::MoveAbs { x: -1920, y: 1080 },
            InjectCmd::Button { button: 2, pressed: true },
            InjectCmd::Scroll { dx: 0, dy: -3 },
            InjectCmd::Key { hid: 0xE3, pressed: false },
            InjectCmd::ReleaseAll,
        ] {
            assert_eq!(format!("{:?}", decode(&encode(&c)).unwrap()), format!("{c:?}"));
        }
        assert!(decode(&[9; MSG]).is_none());
    }
}
