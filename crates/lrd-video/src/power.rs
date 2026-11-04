//! Durante uma sessão: impede que a tela apague/o computador durma, e dá prioridade de tempo
//! real leve à captura (timer de 1 ms e prioridade acima do normal no Windows).

pub struct SessionPower {
    #[cfg(target_os = "macos")]
    caffeinate: Option<std::process::Child>,
}

impl SessionPower {
    pub fn begin() -> Self {
        #[cfg(target_os = "windows")]
        unsafe {
            use windows_sys::Win32::Media::timeBeginPeriod;
            use windows_sys::Win32::System::Power::{
                SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
            };
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
            };
            SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
            timeBeginPeriod(1);
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
            SessionPower {}
        }
        #[cfg(target_os = "macos")]
        {
            let child = std::process::Command::new("/usr/bin/caffeinate")
                .args(["-d", "-i", "-w", &std::process::id().to_string()])
                .spawn()
                .ok();
            SessionPower { caffeinate: child }
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            SessionPower {}
        }
    }
}

impl Drop for SessionPower {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        unsafe {
            use windows_sys::Win32::Media::timeEndPeriod;
            use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
            timeEndPeriod(1);
            SetThreadExecutionState(ES_CONTINUOUS);
        }
        #[cfg(target_os = "macos")]
        if let Some(mut c) = self.caffeinate.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
