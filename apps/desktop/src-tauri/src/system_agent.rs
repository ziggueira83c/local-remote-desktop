//! Windows, acesso total: agente do sistema e ajudante de entrada.
//!
//! - `--system-agent`: aberto pela tarefa agendada "LRD System Agent" como SISTEMA (sessão 0,
//!   sem tela). Mantém um ajudante rodando na sessão de quem está usando o computador.
//! - `--input-helper <pid do agente>`: o ajudante, como SISTEMA na sessão do usuário; recebe o
//!   mouse e o teclado do LRD e os injeta na área de trabalho ativa — inclusive nos pedidos
//!   de permissão do administrador (UAC), que nem um programa elevado consegue controlar.
//!
//! Os dois rodam da cópia protegida em Program Files (ver `admin.rs`).

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, TOKEN_ALL_ACCESS,
    };
    use windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, TerminateProcess,
        WaitForSingleObject, CREATE_NO_WINDOW, INFINITE, PROCESS_INFORMATION, PROCESS_SYNCHRONIZE, STARTUPINFOW,
    };

    const NO_SESSION: u32 = 0xFFFF_FFFF;

    struct Helper {
        process: HANDLE,
        session: u32,
    }

    impl Helper {
        fn alive(&self) -> bool {
            unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
        }
        fn stop(self) {
            unsafe {
                TerminateProcess(self.process, 0);
                CloseHandle(self.process);
            }
        }
    }

    /// Abre o ajudante na sessão `session` com o token do SISTEMA deste processo.
    fn spawn_helper(session: u32) -> Option<Helper> {
        let exe = std::env::current_exe().ok()?;
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let mut cmd = wide(&format!("\"{}\" --input-helper {}", exe.display(), unsafe { GetCurrentProcessId() }));
        let mut desktop = wide("winsta0\\default");
        unsafe {
            let mut own: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) == 0 {
                return None;
            }
            let mut token: HANDLE = std::ptr::null_mut();
            let ok = DuplicateTokenEx(own, TOKEN_ALL_ACCESS, std::ptr::null(), SecurityImpersonation, TokenPrimary, &mut token);
            CloseHandle(own);
            if ok == 0 {
                return None;
            }
            let mut sid = session;
            if SetTokenInformation(token, TokenSessionId, &mut sid as *mut u32 as *mut _, 4) == 0 {
                CloseHandle(token);
                return None;
            }
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = desktop.as_mut_ptr();
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessAsUserW(
                token,
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NO_WINDOW,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            );
            CloseHandle(token);
            if ok == 0 {
                return None;
            }
            CloseHandle(pi.hThread);
            Some(Helper { process: pi.hProcess, session })
        }
    }

    pub fn run_agent() -> ! {
        let mut helper: Option<Helper> = None;
        loop {
            let session = unsafe { WTSGetActiveConsoleSessionId() };
            let ok = helper.as_ref().is_some_and(|h| h.session == session && h.alive());
            if !ok {
                if let Some(h) = helper.take() {
                    h.stop();
                }
                if session != NO_SESSION && session != 0 {
                    helper = spawn_helper(session);
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    pub fn run_helper(agent_pid: u32) -> ! {
        // Sai junto com o agente (acesso total desligado ou tarefa encerrada).
        std::thread::spawn(move || unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, agent_pid);
            if !h.is_null() {
                WaitForSingleObject(h, INFINITE);
            }
            std::process::exit(0);
        });
        lrd_input::elevated::serve()
    }
}

/// Modos especiais do executável; `true` se este processo era um deles (já terminou).
pub fn handle_args() -> bool {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        #[cfg(windows)]
        Some("--system-agent") => imp::run_agent(),
        #[cfg(windows)]
        Some("--input-helper") => imp::run_helper(args.get(2).and_then(|p| p.parse().ok()).unwrap_or(0)),
        Some("--system-agent") | Some("--input-helper") => true,
        _ => false,
    }
}
