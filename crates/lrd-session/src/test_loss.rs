//! Perda de pacotes simulada para testes (`LRD_TEST_DROP_PCT`). Fica entre o cache de
//! retransmissão (NACK) e a rede — como uma perda de verdade no Wi‑Fi: o pacote sumido
//! ainda pode ser reenviado quando o navegador pedir.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use webrtc::interceptor::stream_info::StreamInfo;
use webrtc::interceptor::{
    Attributes, Error, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader, RTPWriter,
};

pub struct LossBuilder {
    pub pct: u32,
}

impl InterceptorBuilder for LossBuilder {
    fn build(&self, _id: &str) -> Result<Arc<dyn Interceptor + Send + Sync>, Error> {
        Ok(Arc::new(Loss { pct: self.pct, state: Arc::new(AtomicU32::new(0x9e37_79b9)) }))
    }
}

struct Loss {
    pct: u32,
    state: Arc<AtomicU32>,
}

struct LossWriter {
    pct: u32,
    state: Arc<AtomicU32>,
    next: Arc<dyn RTPWriter + Send + Sync>,
}

#[async_trait]
impl RTPWriter for LossWriter {
    async fn write(&self, pkt: &webrtc::rtp::packet::Packet, a: &Attributes) -> Result<usize, Error> {
        let mut x = self.state.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state.store(x, Ordering::Relaxed);
        if x % 100 < self.pct {
            return Ok(pkt.payload.len());
        }
        self.next.write(pkt, a).await
    }
}

#[async_trait]
impl Interceptor for Loss {
    async fn bind_rtcp_reader(&self, reader: Arc<dyn RTCPReader + Send + Sync>) -> Arc<dyn RTCPReader + Send + Sync> {
        reader
    }
    async fn bind_rtcp_writer(&self, writer: Arc<dyn RTCPWriter + Send + Sync>) -> Arc<dyn RTCPWriter + Send + Sync> {
        writer
    }
    async fn bind_local_stream(
        &self,
        info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        if !info.mime_type.to_lowercase().starts_with("video") {
            return writer;
        }
        Arc::new(LossWriter { pct: self.pct, state: self.state.clone(), next: writer })
    }
    async fn unbind_local_stream(&self, _info: &StreamInfo) {}
    async fn bind_remote_stream(
        &self,
        _info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        reader
    }
    async fn unbind_remote_stream(&self, _info: &StreamInfo) {}
    async fn close(&self) -> Result<(), Error> {
        Ok(())
    }
}
