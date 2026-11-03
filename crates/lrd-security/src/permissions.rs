//! Perfis e permissões de sessão. Aplicadas SEMPRE no agente do computador controlado (RN-009).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    ViewOnly,
    RemoteControl,
    FullControl,
}

impl Profile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Profile::ViewOnly => "view_only",
            Profile::RemoteControl => "remote_control",
            Profile::FullControl => "full_control",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "view_only" => Some(Profile::ViewOnly),
            "remote_control" => Some(Profile::RemoteControl),
            "full_control" => Some(Profile::FullControl),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Permissions {
    pub view_screen: bool,
    pub control_mouse: bool,
    pub control_keyboard: bool,
    pub sync_clipboard: bool,
    pub transfer_files: bool,
    pub switch_displays: bool,
}

pub const PERM_VIEW_SCREEN: &str = "view_screen";
pub const PERM_CONTROL_MOUSE: &str = "control_mouse";
pub const PERM_CONTROL_KEYBOARD: &str = "control_keyboard";
pub const PERM_SYNC_CLIPBOARD: &str = "sync_clipboard";
pub const PERM_TRANSFER_FILES: &str = "transfer_files";
pub const PERM_SWITCH_DISPLAYS: &str = "switch_displays";

impl Permissions {
    pub fn for_profile(p: Profile) -> Self {
        match p {
            Profile::ViewOnly => Permissions { view_screen: true, switch_displays: true, ..Default::default() },
            Profile::RemoteControl => Permissions {
                view_screen: true,
                control_mouse: true,
                control_keyboard: true,
                switch_displays: true,
                ..Default::default()
            },
            Profile::FullControl => Permissions {
                view_screen: true,
                control_mouse: true,
                control_keyboard: true,
                switch_displays: true,
                sync_clipboard: true,
                transfer_files: true,
            },
        }
    }

    /// Interseção: o controlado nunca concede mais do que foi pedido.
    pub fn intersect(&self, other: &Permissions) -> Permissions {
        Permissions {
            view_screen: self.view_screen && other.view_screen,
            control_mouse: self.control_mouse && other.control_mouse,
            control_keyboard: self.control_keyboard && other.control_keyboard,
            sync_clipboard: self.sync_clipboard && other.sync_clipboard,
            transfer_files: self.transfer_files && other.transfer_files,
            switch_displays: self.switch_displays && other.switch_displays,
        }
    }

    pub fn to_list(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.view_screen {
            v.push(PERM_VIEW_SCREEN.to_string());
        }
        if self.control_mouse {
            v.push(PERM_CONTROL_MOUSE.to_string());
        }
        if self.control_keyboard {
            v.push(PERM_CONTROL_KEYBOARD.to_string());
        }
        if self.sync_clipboard {
            v.push(PERM_SYNC_CLIPBOARD.to_string());
        }
        if self.transfer_files {
            v.push(PERM_TRANSFER_FILES.to_string());
        }
        if self.switch_displays {
            v.push(PERM_SWITCH_DISPLAYS.to_string());
        }
        v
    }

    pub fn from_list(list: &[String]) -> Self {
        let has = |p: &str| list.iter().any(|s| s == p);
        Permissions {
            view_screen: has(PERM_VIEW_SCREEN),
            control_mouse: has(PERM_CONTROL_MOUSE),
            control_keyboard: has(PERM_CONTROL_KEYBOARD),
            sync_clipboard: has(PERM_SYNC_CLIPBOARD),
            transfer_files: has(PERM_TRANSFER_FILES),
            switch_displays: has(PERM_SWITCH_DISPLAYS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_roundtrip() {
        let p = Permissions::for_profile(Profile::RemoteControl);
        assert_eq!(Permissions::from_list(&p.to_list()), p);
    }

    #[test]
    fn intersect_never_grows() {
        let asked = Permissions::for_profile(Profile::ViewOnly);
        let granted = Permissions::for_profile(Profile::FullControl).intersect(&asked);
        assert!(!granted.control_mouse);
    }
}
