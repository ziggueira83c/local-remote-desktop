//! Áudio do sistema. No Windows, loopback WASAPI (o que está tocando nos alto-falantes).
//! No macOS o áudio vem do ScreenCaptureKit, junto com o vídeo (ver `mac_sck`).

use tokio::sync::mpsc;

/// Acumula amostras e envia blocos de ~10 ms em PCM 16 bits estéreo.
pub(crate) struct Chunker {
    rate: u32,
    in_channels: usize,
    buf: Vec<i16>,
    tx: mpsc::Sender<Vec<u8>>,
}

impl Chunker {
    pub(crate) fn new(rate: u32, in_channels: usize, tx: mpsc::Sender<Vec<u8>>) -> Self {
        Chunker { rate, in_channels: in_channels.max(1), buf: Vec::with_capacity(rate as usize / 25), tx }
    }

    /// Recebe amostras intercaladas (qualquer número de canais) já em i16.
    pub(crate) fn push(&mut self, samples: impl Iterator<Item = i16>) {
        let ch = self.in_channels;
        let mut frame = [0i16; 2];
        for (i, s) in samples.enumerate() {
            let c = i % ch;
            if c < 2 {
                frame[c] = s;
            }
            if c == ch - 1 {
                if ch == 1 {
                    frame[1] = frame[0];
                }
                self.buf.extend_from_slice(&frame);
            }
        }
        let chunk = (self.rate as usize / 100) * 2;
        while self.buf.len() >= chunk {
            let rest = self.buf.split_off(chunk);
            let block = std::mem::replace(&mut self.buf, rest);
            let frames = (block.len() / 2) as u64;
            let ts = lrd_protocol::now_us().saturating_sub(frames * 1_000_000 / self.rate as u64);
            let _ = self.tx.try_send(lrd_protocol::video::build_audio_frame(self.rate, 2, ts, &block));
        }
    }
}

/// Captura de áudio em andamento; para ao ser descartada.
pub struct AudioCapture {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Inicia a captura do áudio do sistema (Windows). Em outras plataformas retorna erro.
pub fn start_system_audio(tx: mpsc::Sender<Vec<u8>>) -> anyhow::Result<AudioCapture> {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(target_os = "windows")]
    {
        win::start(tx, stop.clone())?;
        Ok(AudioCapture { stop })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (tx, stop);
        Err(anyhow::anyhow!("captura de áudio por loopback disponível apenas no Windows"))
    }
}

#[cfg(target_os = "windows")]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{Sample, SampleFormat};
    use tokio::sync::mpsc;

    use super::Chunker;

    pub fn start(tx: mpsc::Sender<Vec<u8>>, stop: Arc<AtomicBool>) -> Result<()> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new().name("lrd-audio".into()).spawn(move || {
            let run = || -> Result<cpal::Stream> {
                let host = cpal::default_host();
                let device = host.default_output_device().ok_or_else(|| anyhow!("nenhum dispositivo de saída de áudio"))?;
                let supported = device.default_output_config()?;
                let format = supported.sample_format();
                let config: cpal::StreamConfig = supported.config();
                let channels = config.channels as usize;
                let rate = config.sample_rate.0;
                tracing::info!("áudio do sistema: {rate} Hz, {channels} canais, {format:?}");
                let err_fn = |e| tracing::warn!("erro no áudio: {e}");
                // Abrir um stream de ENTRADA num dispositivo de SAÍDA ativa o loopback WASAPI.
                let stream = match format {
                    SampleFormat::F32 => {
                        let mut ch = Chunker::new(rate, channels, tx.clone());
                        device.build_input_stream(
                            &config,
                            move |data: &[f32], _: &_| ch.push(data.iter().map(|s| s.to_sample::<i16>())),
                            err_fn,
                            None,
                        )?
                    }
                    SampleFormat::I16 => {
                        let mut ch = Chunker::new(rate, channels, tx.clone());
                        device.build_input_stream(&config, move |data: &[i16], _: &_| ch.push(data.iter().copied()), err_fn, None)?
                    }
                    SampleFormat::I32 => {
                        let mut ch = Chunker::new(rate, channels, tx.clone());
                        device.build_input_stream(
                            &config,
                            move |data: &[i32], _: &_| ch.push(data.iter().map(|s| s.to_sample::<i16>())),
                            err_fn,
                            None,
                        )?
                    }
                    other => return Err(anyhow!("formato de áudio não suportado: {other:?}")),
                };
                stream.play()?;
                Ok(stream)
            };
            match run() {
                Ok(stream) => {
                    let _ = ready_tx.send(Ok(()));
                    while !stop.load(Ordering::Relaxed) && !tx.is_closed() {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    drop(stream);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("{e:#}")));
                }
            }
        })?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("o áudio não respondeu")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunker_downmixes_and_chunks() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut c = Chunker::new(1000, 4, tx); // 10 quadros por bloco
        c.push((0..4 * 25).map(|i| i as i16));
        let first = rx.try_recv().unwrap();
        assert_eq!(first[0], lrd_protocol::video::FRAME_AUDIO);
        assert_eq!(first.len(), lrd_protocol::video::AUDIO_HEADER_LEN + 10 * 2 * 2);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err()); // sobram 5 quadros no buffer
    }
}
