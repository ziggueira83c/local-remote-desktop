//! Enquadramento: `u32` big-endian com o tamanho, seguido do conteúdo.

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::messages::{decode_envelope, encode_envelope, Envelope};

/// Escreve um quadro com prefixo de tamanho.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> io::Result<()> {
    let len = u32::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "quadro grande demais"))?;
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf).await?;
    Ok(())
}

/// Lê um quadro com prefixo de tamanho, rejeitando tamanhos acima de `max`.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("quadro de {len} bytes excede o limite de {max}"),
        ));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data).await?;
    Ok(data)
}

/// Escreve um `Envelope` Protobuf.
pub async fn write_envelope<W: AsyncWrite + Unpin>(w: &mut W, env: &Envelope) -> io::Result<()> {
    write_frame(w, &encode_envelope(env)).await
}

/// Lê um `Envelope` Protobuf (limitado a `MAX_CONTROL_MESSAGE`).
pub async fn read_envelope<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Envelope> {
    let data = read_frame(r, crate::MAX_CONTROL_MESSAGE).await?;
    decode_envelope(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip_and_limit() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, b"ola").await.unwrap();
        assert_eq!(read_frame(&mut b, 16).await.unwrap(), b"ola");
        write_frame(&mut a, &[0u8; 64]).await.unwrap();
        assert!(read_frame(&mut b, 16).await.is_err());
    }
}
