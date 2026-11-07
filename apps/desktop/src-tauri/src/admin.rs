//! "Acesso total" no Windows: rodar o LRD como administrador.
//!
//! O Windows impede que um programa comum mande mouse/teclado para janelas de administrador
//! (Gerenciador de Tarefas, instaladores, Regedit…). Para controlá-las, o próprio LRD precisa
//! rodar elevado. Em vez de pedir o UAC a cada abertura, registramos uma tarefa agendada
//! "com privilégios mais altos" (feito uma vez, com um único pedido do UAC); depois o app abre
//! por ela, já elevado, inclusive no logon.
//!
//! Os pedidos do UAC aparecem por padrão na "área de trabalho segura", que nenhum programa
//! do usuário consegue capturar nem controlar. Opcionalmente desligamos isso
//! (`PromptOnSecureDesktop = 0`) para o pedido aparecer na tela normal e poder ser aceito
//! remotamente.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminStatus {
    /// O recurso existe nesta plataforma (só Windows).
    pub supported: bool,
    /// Este processo está rodando como administrador agora.
    pub elevated: bool,
    /// A tarefa "abrir como administrador" está registrada.
    pub enabled: bool,
    /// Os pedidos do UAC aparecem na área de trabalho normal (controláveis remotamente).
    pub uac_on_normal_desktop: bool,
}

#[cfg(not(windows))]
mod imp {
    use super::AdminStatus;

    pub fn status() -> AdminStatus {
        AdminStatus { supported: false, elevated: false, enabled: false, uac_on_normal_desktop: false }
    }
    pub fn enable(_uac_normal: bool) -> Result<(), String> {
        Err("Disponível só no Windows.".into())
    }
    pub fn disable() -> Result<(), String> {
        Err("Disponível só no Windows.".into())
    }
    pub fn relaunch_elevated_later() -> Result<(), String> {
        Err("Disponível só no Windows.".into())
    }
    pub fn redirect_to_elevated() -> bool {
        false
    }
}

#[cfg(windows)]
mod imp {
    use super::AdminStatus;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    const TASK: &str = "LRD Elevated";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const POLICY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
    const ERROR_CANCELLED: u32 = 1223;

    fn hidden(cmd: &str) -> Command {
        let mut c = Command::new(cmd);
        c.creation_flags(CREATE_NO_WINDOW);
        c
    }

    pub fn is_elevated() -> bool {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut el as *mut _ as *mut _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            CloseHandle(token);
            ok != 0 && el.TokenIsElevated != 0
        }
    }

    /// Caminho do executável registrado na tarefa (se existir).
    fn task_command() -> Option<String> {
        let out = hidden("schtasks").args(["/Query", "/TN", TASK, "/XML"]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let xml = decode_text(&out.stdout);
        let a = xml.find("<Command>")? + "<Command>".len();
        let b = xml[a..].find("</Command>")? + a;
        Some(unescape(xml[a..b].trim().trim_matches('"')))
    }

    fn uac_on_normal_desktop() -> bool {
        let Ok(out) = hidden("reg").args(["query", POLICY, "/v", "PromptOnSecureDesktop"]).output() else {
            return false;
        };
        let s = String::from_utf8_lossy(&out.stdout);
        s.lines().any(|l| l.contains("PromptOnSecureDesktop") && l.trim_end().ends_with("0x0"))
    }

    fn same_path(a: &str, b: &Path) -> bool {
        let b = b.to_string_lossy();
        a.trim().eq_ignore_ascii_case(b.trim_start_matches(r"\\?\"))
    }

    pub fn status() -> AdminStatus {
        let exe = std::env::current_exe().ok();
        let enabled = match (task_command(), &exe) {
            (Some(cmd), Some(exe)) => same_path(&cmd, exe),
            (Some(_), None) => true,
            _ => false,
        };
        AdminStatus { supported: true, elevated: is_elevated(), enabled, uac_on_normal_desktop: uac_on_normal_desktop() }
    }

    /// Roda um script .cmd como administrador (um pedido do UAC) e espera terminar.
    fn run_elevated_script(body: &str) -> Result<(), String> {
        let dir = std::env::temp_dir().join(format!("lrd-admin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let script = dir.join("lrd-admin.cmd");
        std::fs::write(&script, format!("@echo off\r\n{body}\r\nexit /b 0\r\n")).map_err(|e| e.to_string())?;
        let params = format!("/c \"\"{}\"\"", script.display());
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (verb, file, params) = (wide("runas"), wide("cmd.exe"), wide(&params));
        let code = unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = params.as_ptr();
            info.nShow = SW_HIDE;
            if ShellExecuteExW(&mut info) == 0 {
                let e = GetLastError();
                let _ = std::fs::remove_dir_all(&dir);
                return Err(if e == ERROR_CANCELLED {
                    "O pedido de administrador do Windows foi cancelado.".into()
                } else {
                    format!("Não foi possível pedir permissão de administrador (erro {e}).")
                });
            }
            let mut code = 1u32;
            if !info.hProcess.is_null() {
                WaitForSingleObject(info.hProcess, INFINITE);
                GetExitCodeProcess(info.hProcess, &mut code);
                CloseHandle(info.hProcess);
            }
            code
        };
        let _ = std::fs::remove_dir_all(&dir);
        if code == 0 {
            Ok(())
        } else {
            Err(format!("O Windows recusou a alteração (código {code})."))
        }
    }

    fn escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    fn unescape(s: &str) -> String {
        s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
    }

    /// Saída do schtasks: UTF-16 (com BOM) ou texto de 8 bits.
    fn decode_text(b: &[u8]) -> String {
        if b.len() >= 2 && b[0] == 0xFF && b[1] == 0xFE {
            let u: Vec<u16> = b[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&u)
        } else {
            String::from_utf8_lossy(b).into_owned()
        }
    }

    fn task_xml(exe: &Path, user: &str) -> String {
        // Prioridade 4 = normal (o padrão de tarefas, 7, deixaria a captura e o vídeo lentos);
        // sem limite de tempo (o padrão encerra a tarefa em 72 h) e rodando também na bateria.
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Local Remote Desktop com acesso total (administrador)</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command></Exec></Actions>
</Task>
"#,
            user = escape(user),
            exe = escape(&exe.to_string_lossy().trim_start_matches(r"\\?\").to_string()),
        )
    }

    fn current_user() -> String {
        let user = std::env::var("USERNAME").unwrap_or_default();
        match std::env::var("USERDOMAIN") {
            Ok(d) if !d.is_empty() => format!("{d}\\{user}"),
            _ => user,
        }
    }

    pub fn enable(uac_normal: bool) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let xml_path: PathBuf = std::env::temp_dir().join(format!("lrd-task-{}.xml", std::process::id()));
        // schtasks /XML exige UTF-16 com BOM.
        let mut bytes = vec![0xFF, 0xFE];
        for u in task_xml(&exe, &current_user()).encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(&xml_path, bytes).map_err(|e| e.to_string())?;
        let mut body = format!("schtasks /Create /TN \"{TASK}\" /XML \"{}\" /F >nul || exit /b 2", xml_path.display());
        body += &format!(
            "\r\nreg add \"{POLICY}\" /v PromptOnSecureDesktop /t REG_DWORD /d {} /f >nul || exit /b 3",
            if uac_normal { 0 } else { 1 }
        );
        let r = run_elevated_script(&body);
        let _ = std::fs::remove_file(&xml_path);
        r
    }

    pub fn disable() -> Result<(), String> {
        let body = format!(
            "schtasks /Delete /TN \"{TASK}\" /F >nul 2>nul\r\nreg add \"{POLICY}\" /v PromptOnSecureDesktop /t REG_DWORD /d 1 /f >nul || exit /b 3"
        );
        run_elevated_script(&body)
    }

    /// Abre o LRD de novo pela tarefa (já como administrador) depois que este processo sair,
    /// para a porta de rede ficar livre.
    pub fn relaunch_elevated_later() -> Result<(), String> {
        hidden("cmd")
            .args(["/c", &format!("ping -n 3 127.0.0.1 >nul & schtasks /Run /TN \"{TASK}\" >nul")])
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Na abertura: acesso total ligado mas aberto sem ser administrador (atalho do menu
    /// Iniciar, reinício depois de atualizar) ⇒ abre pela tarefa e este processo sai.
    pub fn redirect_to_elevated() -> bool {
        if std::env::var_os("LRD_NO_ELEVATE").is_some() || std::env::var_os("LRD_DATA_DIR").is_some() || is_elevated() {
            return false;
        }
        let Ok(exe) = std::env::current_exe() else { return false };
        match task_command() {
            Some(cmd) if same_path(&cmd, &exe) => {
                hidden("schtasks").args(["/Run", "/TN", TASK]).status().is_ok_and(|s| s.success())
            }
            _ => false,
        }
    }
}

pub use imp::{disable, enable, redirect_to_elevated, relaunch_elevated_later, status};
