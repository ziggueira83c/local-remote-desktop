//! Teclado por plataforma.

#[cfg(target_os = "windows")]
pub use win::Keyboard;

#[cfg(target_os = "macos")]
pub use mac::Keyboard;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub use other::Keyboard;

#[cfg(target_os = "windows")]
mod win {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        KEYEVENTF_SCANCODE,
    };

    pub struct Keyboard;

    impl Keyboard {
        pub fn new() -> Self {
            Keyboard
        }

        pub fn key(&mut self, hid: u32, pressed: bool) -> Result<(), String> {
            let (scan, extended) = crate::hid::hid_to_windows_scancode(hid)
                .ok_or_else(|| format!("tecla HID 0x{hid:02X} sem mapeamento no Windows"))?;
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT { wVk: 0, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
                },
            };
            let sent = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
            if sent == 1 {
                Ok(())
            } else {
                // Normal quando a janela em foco é de um processo elevado (UIPI).
                Err("SendInput bloqueado (janela com privilégio de administrador?)".into())
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    pub struct Keyboard {
        flags: CGEventFlags,
    }

    impl Keyboard {
        pub fn new() -> Self {
            Keyboard { flags: CGEventFlags::empty() }
        }

        pub fn key(&mut self, hid: u32, pressed: bool) -> Result<(), String> {
            let code = crate::hid::hid_to_mac_keycode(hid)
                .ok_or_else(|| format!("tecla HID 0x{hid:02X} sem mapeamento no macOS"))?;

            // Mantém o estado dos modificadores para que Cmd+C etc. funcionem.
            let flag = match hid {
                0xE0 | 0xE4 => Some(CGEventFlags::CGEventFlagControl),
                0xE1 | 0xE5 => Some(CGEventFlags::CGEventFlagShift),
                0xE2 | 0xE6 => Some(CGEventFlags::CGEventFlagAlternate),
                0xE3 | 0xE7 => Some(CGEventFlags::CGEventFlagCommand),
                _ => None,
            };
            if let Some(f) = flag {
                if pressed {
                    self.flags.insert(f);
                } else {
                    self.flags.remove(f);
                }
            }

            let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                .map_err(|_| "CGEventSource indisponível".to_string())?;
            let event = CGEvent::new_keyboard_event(source, code, pressed)
                .map_err(|_| "falha ao criar evento de teclado".to_string())?;
            event.set_flags(self.flags);
            event.post(CGEventTapLocation::HID);
            Ok(())
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod other {
    pub struct Keyboard;

    impl Keyboard {
        pub fn new() -> Self {
            Keyboard
        }

        pub fn key(&mut self, hid: u32, _pressed: bool) -> Result<(), String> {
            Err(format!("teclado não suportado nesta plataforma (HID 0x{hid:02X})"))
        }
    }
}
