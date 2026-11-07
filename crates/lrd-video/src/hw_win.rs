//! Windows: H.264/H.265 pela placa de vídeo via Media Foundation (MFT de hardware: NVIDIA
//! NVENC, Intel QuickSync ou AMD AMF). MFTs de hardware são assíncronos: pedem entrada
//! (METransformNeedInput) e avisam quando há saída (METransformHaveOutput).
//!
//! Modo GPU: o MFT recebe o dispositivo D3D11 da captura (IMFDXGIDeviceManager) e lê
//! texturas NV12 preparadas pelo processador de vídeo (`win_gpu`). Sem cópias pela CPU.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

use crate::color::to_nv12;
use crate::frame::{NativeFrame, RawFrame};
use crate::h264::{has_param_sets, Backend, Encoded};
use crate::win_gpu::{Gpu, GpuConverter};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

const MF_EVENT_FLAG_NO_WAIT: u32 = 1;
const MFT_OUTPUT_STREAM_PROVIDES_SAMPLES: u32 = 0x100;

pub struct MfEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    /// O codificador aceitou qualidade por região (CODECAPI_AVEncVideoROIEnabled).
    roi_enabled: bool,
    roi: Option<(u32, u32, u32, u32, i32)>,
    width: u32,
    height: u32,
    fps: u32,
    need_input: u32,
    pending: Vec<Encoded>,
    sequence_header: Vec<u8>,
    provides_samples: bool,
    output_size: u32,
    name: &'static str,
    frame_index: i64,
    hevc: bool,
    gpu: Option<Gpu>,
    converter: Option<GpuConverter>,
    _manager: Option<IMFDXGIDeviceManager>,
}

// SAFETY: o codificador é usado só pela thread de captura que o criou (MTA).
unsafe impl Send for MfEncoder {}

fn frame_pair(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

impl MfEncoder {
    /// `gpu`: modo GPU (entrada em texturas do dispositivo da captura).
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, hevc: bool, gpu: Option<&Gpu>) -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;

            // Procura MFTs de hardware NV12 → H.264/H.265.
            let subtype = if hevc { MFVideoFormat_HEVC } else { MFVideoFormat_H264 };
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
                Some(&input as *const MFT_REGISTER_TYPE_INFO),
                Some(&output as *const MFT_REGISTER_TYPE_INFO),
                &mut activates,
                &mut count,
            )
            .context("MFTEnumEx")?;
            if count == 0 || activates.is_null() {
                if !activates.is_null() {
                    CoTaskMemFree(Some(activates as *const _));
                }
                return Err(anyhow!("nenhum codificador {} de hardware encontrado", if hevc { "H.265" } else { "H.264" }));
            }
            let list: Vec<Option<IMFActivate>> =
                (0..count as usize).map(|i| std::ptr::read(activates.add(i))).collect();
            CoTaskMemFree(Some(activates as *const _));

            let mut last_err = anyhow!("nenhum codificador utilizável");
            for activate in list.into_iter().flatten() {
                match Self::from_activate(&activate, width, height, fps, bitrate, hevc, gpu) {
                    Ok(enc) => return Ok(enc),
                    Err(e) => {
                        tracing::debug!("codificador de hardware recusado: {e:#}");
                        last_err = e;
                    }
                }
            }
            Err(last_err)
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn from_activate(
        activate: &IMFActivate,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        hevc: bool,
        gpu: Option<&Gpu>,
    ) -> Result<Self> {
        let name = unsafe { vendor_name(activate) };
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.context("ActivateObject")?;
        unsafe {
            let attrs = transform.GetAttributes().context("GetAttributes")?;
            // MFTs assíncronos precisam ser destravados antes do uso.
            let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            // Modo GPU: entrega o dispositivo D3D11 ao codificador (antes dos tipos de mídia).
            let mut manager = None;
            if let Some(g) = gpu {
                if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                    return Err(anyhow!("codificador não aceita texturas D3D11"));
                }
                let mut token = 0u32;
                let mut m: Option<IMFDXGIDeviceManager> = None;
                MFCreateDXGIDeviceManager(&mut token, &mut m).context("MFCreateDXGIDeviceManager")?;
                let m = m.ok_or_else(|| anyhow!("sem IMFDXGIDeviceManager"))?;
                m.ResetDevice(&g.device, token).context("ResetDevice")?;
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)
                    .context("MFT_MESSAGE_SET_D3D_MANAGER")?;
                manager = Some(m);
            }

            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, if hevc { &MFVideoFormat_HEVC } else { &MFVideoFormat_H264 })?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_pair(width, height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, frame_pair(fps.max(1), 1))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, frame_pair(1, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            if hevc {
                out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
            } else {
                out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            }
            transform.SetOutputType(0, &out_type, 0).context("SetOutputType")?;

            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_pair(width, height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, frame_pair(fps.max(1), 1))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, frame_pair(1, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
            transform.SetInputType(0, &in_type, 0).context("SetInputType")?;

            let codec: Option<ICodecAPI> = transform.cast().ok();
            let roi_enabled = codec.as_ref().is_some_and(|c| c.SetValue(&CODECAPI_AVEncVideoROIEnabled, &VARIANT::from(1u32)).is_ok());
            tracing::info!("codificador {name}: qualidade por região {}", if roi_enabled { "disponível" } else { "indisponível" });
            if let Some(c) = &codec {
                let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ = c.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
                let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate));
                // GOP longo: quadros completos só quando o controlador pede.
                let _ = c.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps.max(1) * 600));
            }

            let events: IMFMediaEventGenerator = transform.cast().context("MFT não é assíncrono")?;
            let info = transform.GetOutputStreamInfo(0).context("GetOutputStreamInfo")?;
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(MfEncoder {
                transform,
                events,
                codec,
                roi_enabled,
                roi: None,
                width,
                height,
                fps: fps.max(1),
                need_input: 0,
                pending: Vec::new(),
                sequence_header: Vec::new(),
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES != 0,
                output_size: info.cbSize.max(width * height * 2),
                name,
                frame_index: 0,
                hevc,
                gpu: gpu.cloned(),
                converter: None,
                _manager: manager,
            })
        }
    }

    /// Processa os eventos pendentes sem bloquear. Retorna `true` se algum chegou.
    unsafe fn pump(&mut self) -> Result<bool> {
        let mut any = false;
        loop {
            let ev = match unsafe { self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(MF_EVENT_FLAG_NO_WAIT)) } {
                Ok(e) => e,
                Err(_) => return Ok(any), // MF_E_NO_EVENTS_AVAILABLE
            };
            any = true;
            let kind = unsafe { ev.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                unsafe { self.drain_output()? };
            }
        }
    }

    unsafe fn drain_output(&mut self) -> Result<()> {
        let sample = if self.provides_samples {
            None
        } else {
            let s = unsafe { MFCreateSample()? };
            let b = unsafe { MFCreateMemoryBuffer(self.output_size)? };
            unsafe { s.AddBuffer(&b)? };
            Some(s)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
        drop(unsafe { ManuallyDrop::take(&mut buffers[0].pEvents) });
        if let Err(e) = result {
            if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                // Renegociação do tipo de saída: aceita o primeiro tipo disponível.
                if let Ok(t) = unsafe { self.transform.GetOutputAvailableType(0, 0) } {
                    let _ = unsafe { self.transform.SetOutputType(0, &t, 0) };
                }
                return Ok(());
            }
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(());
            }
            return Err(anyhow!("ProcessOutput: {e}"));
        }
        let Some(sample) = sample else { return Ok(()) };
        let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut len))? };
        let mut data = if ptr.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec() };
        let _ = unsafe { buffer.Unlock() };

        // Alguns codificadores só informam SPS/PPS no tipo de saída: anexa nos quadros completos.
        if keyframe && !has_param_sets(&data, self.hevc) {
            if self.sequence_header.is_empty() {
                if let Ok(t) = unsafe { self.transform.GetOutputCurrentType(0) } {
                    if let Ok(size) = unsafe { t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
                        let mut blob = vec![0u8; size as usize];
                        if unsafe { t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None) }.is_ok() {
                            self.sequence_header = blob;
                        }
                    }
                }
            }
            if !self.sequence_header.is_empty() {
                let mut with = self.sequence_header.clone();
                with.extend_from_slice(&data);
                data = with;
            }
        }
        if !data.is_empty() {
            self.pending.push(Encoded { annexb: data, keyframe });
        }
        Ok(())
    }
}

impl Backend for MfEncoder {
    fn encode(&mut self, frame: &RawFrame, force_keyframe: bool) -> Result<Option<Encoded>> {
        if self.gpu.is_none() && (frame.width != self.width || frame.height != self.height) {
            return Err(anyhow!("dimensões mudaram"));
        }
        unsafe {
            // Espera o codificador pedir entrada (até 50 ms).
            let start = Instant::now();
            while self.need_input == 0 {
                if !self.pump()? {
                    if start.elapsed() > Duration::from_millis(50) {
                        // Ocupado: descarta este quadro (o próximo traz a tela atualizada).
                        return Ok(self.pending.pop());
                    }
                    std::thread::sleep(Duration::from_micros(500));
                }
            }

            if force_keyframe {
                if let Some(c) = &self.codec {
                    let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
            }

            let buffer = if let Some(gpu) = self.gpu.clone() {
                let Some(NativeFrame::D3d(g)) = &frame.native else {
                    return Err(anyhow!("modo GPU recebeu quadro da CPU"));
                };
                let input = (g.width, g.height);
                let output = (self.width, self.height);
                if !self.converter.as_ref().is_some_and(|c| c.matches(&g.gpu, input, output)) {
                    self.converter = Some(GpuConverter::new(&gpu, input, output, self.fps)?);
                }
                let tex = self.converter.as_mut().unwrap().convert(g)?;
                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &tex, 0, false)
                    .context("MFCreateDXGISurfaceBuffer")?;
                if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                    if let Ok(len) = b2.GetContiguousLength() {
                        let _ = buffer.SetCurrentLength(len);
                    }
                }
                buffer
            } else {
                let nv12 = to_nv12(frame);
                let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None)?;
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(nv12.len() as u32)?;
                buffer
            };
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            if let (true, Some((x, y, w, h, delta))) = (self.roi_enabled, self.roi) {
                let area = ROI_AREA {
                    rect: windows::Win32::Foundation::RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 },
                    QPDelta: delta,
                };
                let bytes = std::slice::from_raw_parts(&area as *const ROI_AREA as *const u8, std::mem::size_of::<ROI_AREA>());
                let _ = sample.SetBlob(&MFSampleExtension_ROIRectangle, bytes);
            }
            let duration = 10_000_000i64 / self.fps as i64;
            sample.SetSampleTime(self.frame_index * duration)?;
            sample.SetSampleDuration(duration)?;
            self.frame_index += 1;
            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => self.need_input -= 1,
                Err(e) if e.code() == MF_E_NOTACCEPTING => {}
                Err(e) => return Err(anyhow!("ProcessInput: {e}")),
            }

            // Espera a saída deste quadro (codificadores de hardware levam poucos ms).
            let start = Instant::now();
            while self.pending.is_empty() && start.elapsed() < Duration::from_millis(40) {
                if !self.pump()? {
                    std::thread::sleep(Duration::from_micros(300));
                }
            }
        }
        if self.pending.is_empty() {
            return Ok(None);
        }
        // Junta tudo o que saiu (normalmente um quadro).
        let mut annexb = Vec::new();
        let mut keyframe = false;
        for e in self.pending.drain(..) {
            keyframe |= e.keyframe;
            annexb.extend_from_slice(&e.annexb);
        }
        Ok(Some(Encoded { annexb, keyframe }))
    }

    fn set_roi(&mut self, roi: Option<(u32, u32, u32, u32, i32)>) {
        self.roi = roi;
    }

    fn set_bitrate(&mut self, bps: u32) {
        if let Some(c) = &self.codec {
            let _ = unsafe { c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps)) };
        }
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn accepts_native(&self) -> bool {
        self.gpu.is_some()
    }

    fn accepts_cpu(&self) -> bool {
        self.gpu.is_none()
    }

    fn hevc(&self) -> bool {
        self.hevc
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// Nome amigável do fabricante, para o painel de diagnóstico.
unsafe fn vendor_name(activate: &IMFActivate) -> &'static str {
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    if unsafe { activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)) }.is_ok() {
        let name = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]).to_lowercase();
        if name.contains("nvidia") || name.contains("nvenc") {
            return "NVIDIA NVENC";
        }
        if name.contains("intel") || name.contains("quick") {
            return "Intel QuickSync";
        }
        if name.contains("amd") || name.contains("amf") || name.contains("radeon") {
            return "AMD AMF";
        }
    }
    "Media Foundation (hardware)"
}
