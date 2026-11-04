//! macOS: H.264/H.265 pelo VideoToolbox (codificador de hardware da Apple), em modo de tempo
//! real com controle de taxa de baixa latência e sem reordenação de quadros.
//!
//! Os quadros do ScreenCaptureKit chegam como `CVPixelBuffer` (IOSurface, na GPU) e são
//! entregues direto ao VideoToolbox — sem cópia pela CPU.

use std::ffi::c_void;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{CFAllocatorRef, CFRelease, CFTypeRef};
use core_foundation_sys::dictionary::{CFDictionaryGetValue, CFDictionaryRef};

use crate::frame::{PixelFormat, RawFrame};
use crate::h264::{Backend, Encoded};

type OSStatus = i32;
type VTSessionRef = *mut c_void;
type CVPixelBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMFormatDescriptionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const K_CMTIME_FLAGS_VALID: u32 = 1;
const K_CMVIDEO_CODEC_TYPE_H264: u32 = 0x6176_6331; // 'avc1'
const K_CMVIDEO_CODEC_TYPE_HEVC: u32 = 0x6876_6331; // 'hvc1'
const K_CVPIXEL_FORMAT_32BGRA: u32 = 0x4247_5241; // 'BGRA'

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CMSampleBufferRef);

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
    static kVTProfileLevel_HEVC_Main_AutoLevel: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;

    fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFAllocatorRef,
        output_callback: OutputCallback,
        output_callback_refcon: *mut c_void,
        session_out: *mut VTSessionRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: VTSessionRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTSessionRef) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: VTSessionRef,
        image_buffer: CVPixelBufferRef,
        pts: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: VTSessionRef, complete_until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTSessionRef);
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    static kCMTimeInvalid: CMTime;
    fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMBlockBufferGetDataLength(buf: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(buf: CMBlockBufferRef, offset: usize, len: usize, dest: *mut c_void) -> OSStatus;
    fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sbuf: CMSampleBufferRef, create: u8) -> CFArrayRef;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len_out: *mut i32,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len_out: *mut i32,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferCreate(
        allocator: CFAllocatorRef,
        width: usize,
        height: usize,
        pixel_format: u32,
        attributes: CFDictionaryRef,
        out: *mut CVPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(pb: CVPixelBufferRef) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(pb: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetWidth(pb: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pb: CVPixelBufferRef) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
}

/// `CVPixelBuffer` com contagem de referência própria (quadro da tela na GPU).
pub struct PixelBuffer(CVPixelBufferRef);

// SAFETY: CVPixelBuffer é um objeto CoreFoundation com contagem de referência atômica;
// só é lido (bloqueio somente-leitura) depois de capturado.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

impl PixelBuffer {
    /// Retém `ptr` (que continua pertencendo a quem o passou).
    pub unsafe fn retain(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        unsafe { CFRetain(ptr as CFTypeRef) };
        Some(PixelBuffer(ptr))
    }

    pub fn size(&self) -> (u32, u32) {
        unsafe { (CVPixelBufferGetWidth(self.0) as u32, CVPixelBufferGetHeight(self.0) as u32) }
    }

    /// Copia os pixels BGRA para a memória (usado só quando o codificador não é de hardware).
    pub fn read_bgra(&self) -> Option<(u32, u32, Vec<u8>)> {
        const READ_ONLY: u64 = 1;
        unsafe {
            if CVPixelBufferLockBaseAddress(self.0, READ_ONLY) != 0 {
                return None;
            }
            let (w, h) = self.size();
            let bpr = CVPixelBufferGetBytesPerRow(self.0);
            let base = CVPixelBufferGetBaseAddress(self.0) as *const u8;
            let out = if base.is_null() {
                None
            } else {
                let src = std::slice::from_raw_parts(base, bpr * h as usize);
                crate::frame::pack_rows(src, w as usize, h as usize, bpr)
            };
            CVPixelBufferUnlockBaseAddress(self.0, READ_ONLY);
            out.map(|d| (w, h, d))
        }
    }
}

impl Clone for PixelBuffer {
    fn clone(&self) -> Self {
        unsafe { CFRetain(self.0 as CFTypeRef) };
        PixelBuffer(self.0)
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

/// Saídas do callback do VideoToolbox (chamado em outra thread).
struct Sink {
    out: Mutex<Vec<Encoded>>,
    hevc: bool,
}

extern "C" fn on_output(refcon: *mut c_void, _frame: *mut c_void, status: OSStatus, _flags: u32, sample: CMSampleBufferRef) {
    if status != 0 || sample.is_null() || refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` aponta para o `Sink` que vive enquanto a sessão existir.
    let sink = unsafe { &*(refcon as *const Sink) };
    if let Some(e) = unsafe { convert_sample(sample, sink.hevc) } {
        if let Ok(mut v) = sink.out.lock() {
            v.push(e);
        }
    }
}

/// CMSampleBuffer (AVCC/HVCC, tamanhos de 4 bytes) → Annex B, com os conjuntos de
/// parâmetros (SPS/PPS, e VPS no H.265) nos quadros completos.
unsafe fn convert_sample(sample: CMSampleBufferRef, hevc: bool) -> Option<Encoded> {
    unsafe {
        let mut keyframe = true;
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
            let dict = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
            if !dict.is_null() {
                let v = CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync as *const c_void);
                if !v.is_null() {
                    let b: CFBoolean = TCFType::wrap_under_get_rule(v as core_foundation_sys::number::CFBooleanRef);
                    keyframe = !bool::from(b);
                }
            }
        }

        let mut out = Vec::new();
        let mut nal_len_size = 4usize;
        if keyframe {
            let desc = CMSampleBufferGetFormatDescription(sample);
            let mut count = 0usize;
            let mut header = 0i32;
            let mut ptr: *const u8 = std::ptr::null();
            let mut size = 0usize;
            let get = if hevc {
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex
            } else {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex
            };
            if get(desc, 0, &mut ptr, &mut size, &mut count, &mut header) == 0 {
                if header > 0 {
                    nal_len_size = header as usize;
                }
                for i in 0..count {
                    if get(desc, i, &mut ptr, &mut size, std::ptr::null_mut(), std::ptr::null_mut()) == 0
                        && !ptr.is_null()
                    {
                        out.extend_from_slice(&[0, 0, 0, 1]);
                        out.extend_from_slice(std::slice::from_raw_parts(ptr, size));
                    }
                }
            }
        }

        let block = CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            return None;
        }
        let len = CMBlockBufferGetDataLength(block);
        let mut avcc = vec![0u8; len];
        if CMBlockBufferCopyDataBytes(block, 0, len, avcc.as_mut_ptr() as *mut c_void) != 0 {
            return None;
        }
        let mut i = 0;
        while i + nal_len_size <= avcc.len() {
            let mut n = 0usize;
            for k in 0..nal_len_size {
                n = (n << 8) | avcc[i + k] as usize;
            }
            i += nal_len_size;
            if n == 0 || i + n > avcc.len() {
                break;
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&avcc[i..i + n]);
            i += n;
        }
        Some(Encoded { annexb: out, keyframe })
    }
}

pub struct VtEncoder {
    session: VTSessionRef,
    sink: Box<Sink>,
    width: u32,
    height: u32,
    fps: u32,
    hevc: bool,
}

// SAFETY: a sessão é usada só pela thread de captura; o callback acessa apenas o `Sink`.
unsafe impl Send for VtEncoder {}

fn set_prop(session: VTSessionRef, key: CFStringRef, value: &CFType) {
    unsafe {
        let _ = VTSessionSetProperty(session, key, value.as_CFTypeRef());
    }
}

impl VtEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, hevc: bool) -> Result<Self> {
        let sink = Box::new(Sink { out: Mutex::new(Vec::new()), hevc });
        let mut session: VTSessionRef = std::ptr::null_mut();
        unsafe {
            let spec = CFDictionary::from_CFType_pairs(&[(
                CFString::wrap_under_get_rule(kVTVideoEncoderSpecification_EnableLowLatencyRateControl),
                CFBoolean::true_value().as_CFType(),
            )]);
            let codec = if hevc { K_CMVIDEO_CODEC_TYPE_HEVC } else { K_CMVIDEO_CODEC_TYPE_H264 };
            let create = |spec: CFDictionaryRef, session: &mut VTSessionRef| {
                VTCompressionSessionCreate(
                    std::ptr::null(),
                    width as i32,
                    height as i32,
                    codec,
                    spec,
                    std::ptr::null(),
                    std::ptr::null(),
                    on_output,
                    &*sink as *const Sink as *mut c_void,
                    session,
                )
            };
            let mut status = create(spec.as_concrete_TypeRef(), &mut session);
            if status != 0 || session.is_null() {
                // O modo de baixa latência não existe para todos os codecs/Macs.
                session = std::ptr::null_mut();
                status = create(std::ptr::null(), &mut session);
            }
            if status != 0 || session.is_null() {
                return Err(anyhow!("VTCompressionSessionCreate falhou ({status})"));
            }
            set_prop(session, kVTCompressionPropertyKey_RealTime, &CFBoolean::true_value().as_CFType());
            let profile = if hevc { kVTProfileLevel_HEVC_Main_AutoLevel } else { kVTProfileLevel_H264_Baseline_AutoLevel };
            set_prop(session, kVTCompressionPropertyKey_ProfileLevel, &CFString::wrap_under_get_rule(profile).as_CFType());
            set_prop(session, kVTCompressionPropertyKey_AllowFrameReordering, &CFBoolean::false_value().as_CFType());
            set_prop(session, kVTCompressionPropertyKey_AverageBitRate, &CFNumber::from(bitrate as i32).as_CFType());
            set_prop(session, kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::from(fps.max(1) as i32).as_CFType());
            set_prop(
                session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                &CFNumber::from((fps.max(1) * 600) as i32).as_CFType(),
            );
            VTCompressionSessionPrepareToEncodeFrames(session);
        }
        Ok(VtEncoder { session, sink, width, height, fps: fps.max(1), hevc })
    }
}

impl Backend for VtEncoder {
    fn encode(&mut self, frame: &RawFrame, force_keyframe: bool) -> Result<Option<Encoded>> {
        if frame.width != self.width || frame.height != self.height {
            return Err(anyhow!("dimensões mudaram"));
        }
        unsafe {
            // Caminho direto: o quadro já está na GPU (ScreenCaptureKit).
            let native = match &frame.native {
                Some(crate::frame::NativeFrame::Cv(pb)) => Some(pb.clone()),
                _ => None,
            };
            let mut pb: CVPixelBufferRef = std::ptr::null_mut();
            let owned = if let Some(n) = &native {
                pb = n.0;
                false
            } else {
                true
            };
            if owned {
            let rc = CVPixelBufferCreate(
                std::ptr::null(),
                frame.width as usize,
                frame.height as usize,
                K_CVPIXEL_FORMAT_32BGRA,
                std::ptr::null(),
                &mut pb,
            );
            if rc != 0 || pb.is_null() {
                return Err(anyhow!("CVPixelBufferCreate falhou ({rc})"));
            }
            CVPixelBufferLockBaseAddress(pb, 0);
            let base = CVPixelBufferGetBaseAddress(pb) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(pb);
            let row = frame.width as usize * 4;
            for y in 0..frame.height as usize {
                let src = &frame.data[y * row..(y + 1) * row];
                let dst = std::slice::from_raw_parts_mut(base.add(y * stride), row);
                match frame.format {
                    PixelFormat::Bgra => dst.copy_from_slice(src),
                    PixelFormat::Rgba => {
                        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                            d[0] = s[2];
                            d[1] = s[1];
                            d[2] = s[0];
                            d[3] = s[3];
                        }
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(pb, 0);
            }

            let pts = CMTime { value: frame.timestamp_us as i64, timescale: 1_000_000, flags: K_CMTIME_FLAGS_VALID, epoch: 0 };
            let duration = CMTime { value: 1, timescale: self.fps as i32, flags: K_CMTIME_FLAGS_VALID, epoch: 0 };
            let props = if force_keyframe {
                Some(CFDictionary::from_CFType_pairs(&[(
                    CFString::wrap_under_get_rule(kVTEncodeFrameOptionKey_ForceKeyFrame),
                    CFBoolean::true_value().as_CFType(),
                )]))
            } else {
                None
            };
            let status = VTCompressionSessionEncodeFrame(
                self.session,
                pb,
                pts,
                duration,
                props.as_ref().map(|d| d.as_concrete_TypeRef()).unwrap_or(std::ptr::null()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if owned {
                CFRelease(pb as CFTypeRef);
            }
            drop(native);
            if status != 0 {
                return Err(anyhow!("VTCompressionSessionEncodeFrame falhou ({status})"));
            }
            // Força a saída imediata deste quadro (sem acumular).
            VTCompressionSessionCompleteFrames(self.session, kCMTimeInvalid);
        }
        let mut out = self.sink.out.lock().map_err(|_| anyhow!("sink envenenado"))?;
        if out.is_empty() {
            return Ok(None);
        }
        let mut annexb = Vec::new();
        let mut keyframe = false;
        for e in out.drain(..) {
            keyframe |= e.keyframe;
            annexb.extend_from_slice(&e.annexb);
        }
        Ok(Some(Encoded { annexb, keyframe }))
    }

    fn set_bitrate(&mut self, bps: u32) {
        set_prop(self.session, unsafe { kVTCompressionPropertyKey_AverageBitRate }, &CFNumber::from(bps as i32).as_CFType());
    }

    fn name(&self) -> &'static str {
        "Apple VideoToolbox"
    }

    fn accepts_native(&self) -> bool {
        true
    }

    fn hevc(&self) -> bool {
        self.hevc
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, kCMTimeInvalid);
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session as CFTypeRef);
        }
    }
}
