//! Windows: captura e preparo do quadro inteiramente na placa de vídeo.
//!
//! DXGI Desktop Duplication entrega a tela como textura D3D11; ela é copiada (na GPU) para
//! uma textura própria e liberada na hora. O codificador (`hw_win`, no modo GPU) usa o
//! processador de vídeo do D3D11 para reduzir e converter para NV12 e entrega a textura ao
//! NVENC/QuickSync/AMF pelo gerenciador DXGI do Media Foundation. Nenhum pixel passa pela CPU.
//!
//! Se algo falhar (driver, sessão remota, tela protegida), `source.rs` volta para a captura
//! DXGI do `xcap` com cópia para a memória.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use crate::capture::Display;
use crate::frame::{NativeFrame, PixelFormat, RawFrame};

/// Dispositivo D3D11 compartilhado entre captura e codificador.
#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

// SAFETY: o dispositivo é criado com proteção multithread ligada (ID3D11Multithread).
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

/// Quadro na GPU: textura BGRA na resolução da tela.
#[derive(Clone)]
pub struct GpuFrame {
    pub gpu: Gpu,
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    staging: Arc<Mutex<Option<ID3D11Texture2D>>>,
}

unsafe impl Send for GpuFrame {}
unsafe impl Sync for GpuFrame {}

impl GpuFrame {
    pub fn device_key(&self) -> usize {
        self.gpu.device.as_raw() as usize
    }

    /// Copia para a memória (só quando o codificador em uso não aceita texturas).
    pub fn read_bgra(&self) -> Option<(u32, u32, Vec<u8>)> {
        unsafe {
            let mut guard = self.staging.lock().ok()?;
            if guard.is_none() {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: self.width,
                    Height: self.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut t = None;
                self.gpu.device.CreateTexture2D(&desc, None, Some(&mut t)).ok()?;
                *guard = t;
            }
            let staging = guard.as_ref()?;
            self.gpu.context.CopyResource(staging, &self.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.gpu.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
            let (w, h) = (self.width as usize, self.height as usize);
            let pitch = mapped.RowPitch as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h);
            let out = crate::frame::pack_rows(src, w, h, pitch);
            self.gpu.context.Unmap(staging, 0);
            out.map(|d| (self.width, self.height, d))
        }
    }
}

pub struct DuplSource {
    gpu: Gpu,
    output: IDXGIOutput1,
    dupl: Option<IDXGIOutputDuplication>,
    latest: Option<ID3D11Texture2D>,
    size: (u32, u32),
    /// Ainda não entregou nenhuma imagem desde a (re)criação da duplicação.
    need_first: bool,
    staging: Arc<Mutex<Option<ID3D11Texture2D>>>,
    pub display: Display,
}

unsafe impl Send for DuplSource {}

impl DuplSource {
    pub fn open(display: &Display) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let mut found: Option<(IDXGIAdapter1, IDXGIOutput)> = None;
            let mut first: Option<(IDXGIAdapter1, IDXGIOutput)> = None;
            let mut outputs = 0;
            let mut a = 0;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                let mut o = 0;
                while let Ok(output) = adapter.EnumOutputs(o) {
                    outputs += 1;
                    if let Ok(desc) = output.GetDesc() {
                        let r = desc.DesktopCoordinates;
                        if r.left == display.x && r.top == display.y && found.is_none() {
                            found = Some((adapter.clone(), output.clone()));
                        }
                    }
                    if first.is_none() {
                        first = Some((adapter.clone(), output.clone()));
                    }
                    o += 1;
                }
                a += 1;
            }
            let (adapter, output) = match found {
                Some(v) => v,
                None if outputs == 1 => first.unwrap(),
                None => return Err(anyhow!("monitor não encontrado no DXGI")),
            };
            let output: IDXGIOutput1 = output.cast().context("IDXGIOutput1")?;

            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device: ID3D11Device = device.ok_or_else(|| anyhow!("sem dispositivo D3D11"))?;
            let context: ID3D11DeviceContext = context.ok_or_else(|| anyhow!("sem contexto D3D11"))?;
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            // Prioridade alta da GPU para a captura (não espera jogos/vídeos na fila).
            if let Ok(dx) = device.cast::<IDXGIDevice>() {
                let _ = dx.SetGPUThreadPriority(7);
            }
            let mut s = DuplSource {
                gpu: Gpu { device, context },
                output,
                dupl: None,
                latest: None,
                size: (0, 0),
                staging: Arc::new(Mutex::new(None)),
                display: display.clone(),
                need_first: true,
            };
            s.duplicate()?;
            Ok(s)
        }
    }

    fn duplicate(&mut self) -> Result<()> {
        self.dupl = None;
        let d = unsafe { self.output.DuplicateOutput(&self.gpu.device) }.context("DuplicateOutput")?;
        self.dupl = Some(d);
        self.need_first = true;
        Ok(())
    }

    /// Próximo quadro (o mais recente). `None` se a tela não mudou dentro de `timeout`.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>> {
        if self.dupl.is_none() {
            self.duplicate()?;
        }
        let dupl = self.dupl.as_ref().unwrap().clone();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let ms = timeout.as_millis().clamp(0, 1000) as u32;
        match unsafe { dupl.AcquireNextFrame(ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Troca de modo de vídeo, UAC, tela cheia exclusiva: recria a duplicação.
                self.dupl = None;
                return Ok(None);
            }
            Err(e) => return Err(anyhow!("AcquireNextFrame: {e}")),
        }
        let result = self.copy_frame(&info, resource);
        let _ = unsafe { dupl.ReleaseFrame() };
        result
    }

    fn copy_frame(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, resource: Option<IDXGIResource>) -> Result<Option<RawFrame>> {
        // Só o mouse mudou: nada de imagem nova (o cursor é desenhado no controlador).
        // Exceção: logo após abrir, a superfície já tem a tela atual — usa para não começar
        // com tela preta esperando algo mudar.
        if (info.LastPresentTime == 0 || info.AccumulatedFrames == 0) && !self.need_first {
            return Ok(None);
        }
        self.need_first = false;
        let Some(resource) = resource else { return Ok(None) };
        let src: ID3D11Texture2D = resource.cast().context("textura da tela")?;
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut desc);
            if self.latest.is_none() || self.size != (desc.Width, desc.Height) {
                let own = D3D11_TEXTURE2D_DESC {
                    Width: desc.Width,
                    Height: desc.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: desc.Format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                self.gpu.device.CreateTexture2D(&own, None, Some(&mut t)).context("CreateTexture2D")?;
                self.latest = t;
                self.size = (desc.Width, desc.Height);
                *self.staging.lock().unwrap() = None;
            }
            let latest = self.latest.as_ref().unwrap();
            self.gpu.context.CopyResource(latest, &src);
            Ok(Some(RawFrame {
                width: desc.Width,
                height: desc.Height,
                data: Vec::new(),
                format: PixelFormat::Bgra,
                timestamp_us: lrd_protocol::now_us(),
                native: Some(NativeFrame::D3d(GpuFrame {
                    gpu: self.gpu.clone(),
                    texture: latest.clone(),
                    width: desc.Width,
                    height: desc.Height,
                    staging: self.staging.clone(),
                })),
            }))
        }
    }
}

/// Redução + BGRA→NV12 no processador de vídeo da GPU, num anel de texturas NV12.
pub struct GpuConverter {
    gpu: Gpu,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: (u32, u32),
    output: (u32, u32),
    ring: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    input_view: Option<(usize, ID3D11VideoProcessorInputView)>,
}

unsafe impl Send for GpuConverter {}

const RING: usize = 4;

impl GpuConverter {
    pub fn new(gpu: &Gpu, input: (u32, u32), output: (u32, u32), fps: u32) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice = gpu.device.cast().context("ID3D11VideoDevice")?;
            let video_context: ID3D11VideoContext = gpu.context.cast().context("ID3D11VideoContext")?;
            let rate = DXGI_RATIONAL { Numerator: fps.max(1), Denominator: 1 };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: rate,
                OutputWidth: output.0,
                OutputHeight: output.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&content).context("enumerador de vídeo")?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0).context("processador de vídeo")?;

            // Entrada RGB faixa completa → saída BT.601 faixa limitada (igual ao caminho da CPU).
            let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let output_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 };
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input_cs);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &output_cs);
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);

            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: output.0,
                    Height: output.1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                gpu.device.CreateTexture2D(&desc, None, Some(&mut t)).context("textura NV12")?;
                let t = t.ok_or_else(|| anyhow!("textura NV12"))?;
                let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                };
                let mut view = None;
                video_device
                    .CreateVideoProcessorOutputView(&t, &enumerator, &view_desc, Some(&mut view))
                    .context("visão de saída")?;
                ring.push((t, view.ok_or_else(|| anyhow!("visão de saída"))?));
            }
            Ok(GpuConverter {
                gpu: gpu.clone(),
                video_device,
                video_context,
                enumerator,
                processor,
                input,
                output,
                ring,
                next: 0,
                input_view: None,
            })
        }
    }

    pub fn matches(&self, gpu: &Gpu, input: (u32, u32), output: (u32, u32)) -> bool {
        self.gpu.device.as_raw() == gpu.device.as_raw() && self.input == input && self.output == output
    }

    /// Converte `frame` para NV12 numa textura do anel e devolve essa textura.
    pub fn convert(&mut self, frame: &GpuFrame) -> Result<ID3D11Texture2D> {
        unsafe {
            let key = frame.texture.as_raw() as usize;
            if self.input_view.as_ref().map(|(k, _)| *k) != Some(key) {
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                    },
                };
                let mut view = None;
                self.video_device
                    .CreateVideoProcessorInputView(&frame.texture, &self.enumerator, &desc, Some(&mut view))
                    .context("visão de entrada")?;
                self.input_view = Some((key, view.ok_or_else(|| anyhow!("visão de entrada"))?));
            }
            let input_view = self.input_view.as_ref().unwrap().1.clone();
            let (texture, out_view) = self.ring[self.next].clone();
            self.next = (self.next + 1) % self.ring.len();
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
                ..Default::default()
            };
            let r = self.video_context.VideoProcessorBlt(&self.processor, &out_view, 0, std::slice::from_ref(&stream));
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            r.context("VideoProcessorBlt")?;
            Ok(texture)
        }
    }
}
