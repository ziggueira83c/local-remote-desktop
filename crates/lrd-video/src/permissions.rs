//! Permissão de Gravação de Tela (macOS). Nos demais sistemas sempre retorna `true`.

#[cfg(target_os = "macos")]
mod mac {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    pub fn has() -> bool {
        unsafe { CGPreflightScreenCaptureAccess() }
    }

    pub fn request() -> bool {
        unsafe { CGRequestScreenCaptureAccess() }
    }
}

/// O processo pode capturar a tela?
pub fn has_screen_capture_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        mac::has()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Pede a permissão ao sistema (abre o aviso do macOS na primeira vez).
pub fn request_screen_capture_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        mac::request()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}
