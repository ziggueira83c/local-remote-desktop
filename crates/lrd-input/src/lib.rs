//! Injeção de mouse e teclado no computador controlado.
//!
//! - Teclado: teclas FÍSICAS (USB HID usage) convertidas em scancodes (Windows, `SendInput`
//!   com `KEYEVENTF_SCANCODE`) ou keycodes virtuais (macOS, `CGEvent`). Assim o layout do
//!   computador controlado (ABNT2, US...) decide o caractere produzido.
//! - Mouse: posição absoluta em coordenadas do SO; botões e rolagem via `enigo`.
//! - Ao encerrar a sessão, tudo que ficou pressionado é liberado (RN-014).

pub mod hid;
mod keyboard;

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::Result;
use enigo::{Axis, Button, Direction, Enigo, Mouse, Settings};

#[derive(Debug, Clone, Copy)]
pub enum InjectCmd {
    /// Posição absoluta no sistema de coordenadas do SO.
    MoveAbs { x: i32, y: i32 },
    /// 0 = esquerdo, 1 = meio, 2 = direito.
    Button { button: u32, pressed: bool },
    Scroll { dx: i32, dy: i32 },
    Key { hid: u32, pressed: bool },
    ReleaseAll,
}

/// Thread dedicada de injeção (o `Enigo` não deve trocar de thread no macOS).
pub struct InputInjector {
    tx: Sender<InjectCmd>,
}

impl InputInjector {
    pub fn start() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("lrd-input".into())
            .spawn(move || run(rx, ready_tx))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(InputInjector { tx }),
            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
            Err(_) => Err(anyhow::anyhow!("thread de entrada encerrou")),
        }
    }

    pub fn send(&self, cmd: InjectCmd) {
        let _ = self.tx.send(cmd);
    }
}

fn run(rx: Receiver<InjectCmd>, ready: Sender<Result<(), String>>) {
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => {
            let _ = ready.send(Err(format!("não foi possível iniciar a injeção de entrada: {e:?}")));
            return;
        }
    };
    let mut kb = keyboard::Keyboard::new();
    let _ = ready.send(Ok(()));

    let mut keys_down: HashSet<u32> = HashSet::new();
    let mut buttons_down: HashSet<u32> = HashSet::new();

    while let Ok(cmd) = rx.recv() {
        let result: Result<(), String> = match cmd {
            InjectCmd::MoveAbs { x, y } => move_abs(&mut enigo, x, y),
            InjectCmd::Button { button, pressed } => {
                let b = match button {
                    1 => Button::Middle,
                    2 => Button::Right,
                    _ => Button::Left,
                };
                if pressed {
                    buttons_down.insert(button);
                } else {
                    buttons_down.remove(&button);
                }
                enigo
                    .button(b, if pressed { Direction::Press } else { Direction::Release })
                    .map_err(|e| format!("{e:?}"))
            }
            InjectCmd::Scroll { dx, dy } => {
                let mut r = Ok(());
                if dy != 0 {
                    r = enigo.scroll(dy.clamp(-20, 20), Axis::Vertical).map_err(|e| format!("{e:?}"));
                }
                if dx != 0 && r.is_ok() {
                    r = enigo.scroll(dx.clamp(-20, 20), Axis::Horizontal).map_err(|e| format!("{e:?}"));
                }
                r
            }
            InjectCmd::Key { hid, pressed } => {
                // Ignora repetição de "solto" para tecla que não está pressionada.
                if !pressed && !keys_down.contains(&hid) {
                    Ok(())
                } else {
                    if pressed {
                        keys_down.insert(hid);
                    } else {
                        keys_down.remove(&hid);
                    }
                    kb.key(hid, pressed)
                }
            }
            InjectCmd::ReleaseAll => {
                for hid in keys_down.drain() {
                    let _ = kb.key(hid, false);
                }
                for b in buttons_down.drain() {
                    let btn = match b {
                        1 => Button::Middle,
                        2 => Button::Right,
                        _ => Button::Left,
                    };
                    let _ = enigo.button(btn, Direction::Release);
                }
                Ok(())
            }
        };
        if let Err(e) = result {
            tracing::debug!("falha na injeção de {cmd:?}: {e}");
        }
    }
}

#[cfg(target_os = "windows")]
fn move_abs(_enigo: &mut Enigo, x: i32, y: i32) -> Result<(), String> {
    // SetCursorPos usa coordenadas do desktop virtual (todos os monitores), em pixels físicos
    // quando o processo tem DPI awareness — o mesmo sistema de coordenadas da captura.
    let ok = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y) };
    if ok != 0 {
        Ok(())
    } else {
        Err("SetCursorPos falhou".into())
    }
}

#[cfg(not(target_os = "windows"))]
fn move_abs(driver: &mut Enigo, x: i32, y: i32) -> Result<(), String> {
    driver.move_mouse(x, y, enigo::Coordinate::Abs).map_err(|e| format!("{e:?}"))
}

/// Posição global atual do cursor, no mesmo sistema de coordenadas de `MoveAbs`.
pub fn cursor_position() -> Option<(i32, i32)> {
    #[cfg(target_os = "windows")]
    {
        let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        let ok = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p) };
        if ok != 0 {
            Some((p.x, p.y))
        } else {
            None
        }
    }
    #[cfg(target_os = "macos")]
    {
        use core_graphics::event::CGEvent;
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        let src = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
        let ev = CGEvent::new(src).ok()?;
        let p = ev.location();
        Some((p.x.round() as i32, p.y.round() as i32))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

/// Formato e visibilidade do cursor (ver `lrd_protocol::video::cursor_kind`).
/// No Windows identifica os cursores padrão do sistema; nos demais, seta padrão.
pub fn cursor_shape() -> (u8, bool) {
    #[cfg(target_os = "windows")]
    {
        win_cursor::shape()
    }
    #[cfg(not(target_os = "windows"))]
    {
        (0, true)
    }
}

#[cfg(target_os = "windows")]
mod win_cursor {
    use std::sync::OnceLock;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorInfo, LoadCursorW, CURSORINFO, CURSOR_SHOWING, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND,
        IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT,
    };

    fn table() -> &'static Vec<(usize, u8)> {
        static T: OnceLock<Vec<(usize, u8)>> = OnceLock::new();
        T.get_or_init(|| {
            let ids = [
                (IDC_ARROW, 0u8),
                (IDC_IBEAM, 1),
                (IDC_HAND, 2),
                (IDC_WAIT, 3),
                (IDC_APPSTARTING, 4),
                (IDC_CROSS, 5),
                (IDC_SIZEWE, 6),
                (IDC_SIZENS, 7),
                (IDC_SIZENWSE, 8),
                (IDC_SIZENESW, 9),
                (IDC_SIZEALL, 10),
                (IDC_NO, 11),
                (IDC_HELP, 12),
            ];
            ids.iter()
                .map(|(id, kind)| (unsafe { LoadCursorW(std::ptr::null_mut(), *id) } as usize, *kind))
                .filter(|(h, _)| *h != 0)
                .collect()
        })
    }

    pub fn shape() -> (u8, bool) {
        let mut ci: CURSORINFO = unsafe { std::mem::zeroed() };
        ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
        if unsafe { GetCursorInfo(&mut ci) } == 0 {
            return (0, true);
        }
        let visible = ci.flags & CURSOR_SHOWING != 0;
        let handle = ci.hCursor as usize;
        let kind = table().iter().find(|(h, _)| *h == handle).map(|(_, k)| *k).unwrap_or(0);
        (kind, visible)
    }
}

/// macOS: o app tem permissão de Acessibilidade (necessária para controlar mouse/teclado)?
pub fn has_accessibility_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        #[link(name = "ApplicationServices", kind = "framework")]
        extern "C" {
            fn AXIsProcessTrusted() -> bool;
        }
        unsafe { AXIsProcessTrusted() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// macOS: abre Ajustes do Sistema › Privacidade › Acessibilidade.
pub fn open_accessibility_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn();
    }
}

/// macOS: abre Ajustes do Sistema › Privacidade › Gravação de Tela.
pub fn open_screen_recording_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
            .spawn();
    }
}
