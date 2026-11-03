//! Enumeração de monitores e captura de quadros.

use anyhow::{anyhow, Result};
use serde::Serialize;

/// Monitor físico. `x`, `y`, `width`, `height` estão no sistema de coordenadas nativo do SO
/// usado pela injeção de mouse: pixels físicos do desktop virtual no Windows (app com DPI
/// awareness) e pontos globais no macOS.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Display {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub primary: bool,
}

/// Quadro bruto RGBA.
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn list_displays() -> Result<Vec<Display>> {
    let monitors = xcap::Monitor::all().map_err(|e| anyhow!("listando monitores: {e}"))?;
    let mut out: Vec<Display> = monitors
        .iter()
        .map(|m| Display {
            id: m.id(),
            name: m.name().to_string(),
            x: m.x(),
            y: m.y(),
            width: m.width(),
            height: m.height(),
            scale_factor: m.scale_factor(),
            primary: m.is_primary(),
        })
        .collect();
    out.sort_by_key(|d| (!d.primary, d.x, d.y));
    Ok(out)
}

/// Captura repetida de um monitor.
pub struct Capturer {
    monitor: xcap::Monitor,
    pub display: Display,
}

impl Capturer {
    /// Abre o monitor `id`; se não existir, usa o principal.
    pub fn open(id: Option<u32>) -> Result<Self> {
        let monitors = xcap::Monitor::all().map_err(|e| anyhow!("listando monitores: {e}"))?;
        let chosen = id
            .and_then(|id| monitors.iter().position(|m| m.id() == id))
            .or_else(|| monitors.iter().position(|m| m.is_primary()))
            .or(if monitors.is_empty() { None } else { Some(0) })
            .ok_or_else(|| anyhow!("nenhum monitor encontrado"))?;
        let monitor = monitors.into_iter().nth(chosen).expect("índice válido");
        let display = Display {
            id: monitor.id(),
            name: monitor.name().to_string(),
            x: monitor.x(),
            y: monitor.y(),
            width: monitor.width(),
            height: monitor.height(),
            scale_factor: monitor.scale_factor(),
            primary: monitor.is_primary(),
        };
        Ok(Capturer { monitor, display })
    }

    pub fn capture(&self) -> Result<RawFrame> {
        let img = self.monitor.capture_image().map_err(|e| anyhow!("capturando tela: {e}"))?;
        let width = img.width();
        let height = img.height();
        let rgba = img.into_raw();
        if rgba.len() != (width as usize) * (height as usize) * 4 {
            return Err(anyhow!("quadro com tamanho inesperado"));
        }
        Ok(RawFrame { width, height, rgba })
    }
}
