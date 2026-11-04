//! Enumeração de monitores e captura de quadros.

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::frame::{PixelFormat, RawFrame};

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

fn describe(m: &xcap::Monitor) -> Result<Display> {
    let err = |e: xcap::XCapError| anyhow!("lendo dados do monitor: {e}");
    Ok(Display {
        id: m.id().map_err(err)?,
        name: m.name().unwrap_or_default(),
        x: m.x().map_err(err)?,
        y: m.y().map_err(err)?,
        width: m.width().map_err(err)?,
        height: m.height().map_err(err)?,
        scale_factor: m.scale_factor().unwrap_or(1.0),
        primary: m.is_primary().unwrap_or(false),
    })
}

pub fn list_displays() -> Result<Vec<Display>> {
    let monitors = xcap::Monitor::all().map_err(|e| anyhow!("listando monitores: {e}"))?;
    let mut out: Vec<Display> = monitors.iter().filter_map(|m| describe(m).ok()).collect();
    out.sort_by_key(|d| (!d.primary, d.x, d.y));
    Ok(out)
}

/// Captura por cópia (reserva quando a captura contínua não está disponível).
pub struct Capturer {
    pub(crate) monitor: xcap::Monitor,
    pub display: Display,
}

impl Capturer {
    /// Abre o monitor `id`; se não existir, usa o principal.
    pub fn open(id: Option<u32>) -> Result<Self> {
        let monitors = xcap::Monitor::all().map_err(|e| anyhow!("listando monitores: {e}"))?;
        let mut described: Vec<(xcap::Monitor, Display)> = monitors
            .into_iter()
            .filter_map(|m| describe(&m).ok().map(|d| (m, d)))
            .collect();
        let chosen = id
            .and_then(|id| described.iter().position(|(_, d)| d.id == id))
            .or_else(|| described.iter().position(|(_, d)| d.primary))
            .or(if described.is_empty() { None } else { Some(0) })
            .ok_or_else(|| anyhow!("nenhum monitor encontrado"))?;
        let (monitor, display) = described.swap_remove(chosen);
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
        Ok(RawFrame { width, height, data: rgba, format: PixelFormat::Rgba, timestamp_us: lrd_protocol::now_us() })
    }
}
