pub mod follower;
pub mod leader;

use crate::server::health::Health;
use crate::transport::Transport;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::server::error::{KeyzError, Result};

pub const SYNC_COMPLETE_MARKER: u8 = 0xFF;
pub const MAX_REPLICATION_FRAME: usize = 64 * 1024 * 1024; // 64 MiB, excluding the frame prefix.
/// SET opcode, two lengths, expiration, compression flag, and CRC32.
pub const REPLICATION_SET_OVERHEAD: usize = 22;

#[derive(Clone)]
pub struct ReplicationOptions {
    pub transport: Transport,
    pub auth_token: Option<String>,
    pub max_connections: usize,
    pub max_snapshot_bytes: u64,
    pub write_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub health: Health,
}

pub async fn write_replication_frame<S: AsyncWrite + Unpin + ?Sized>(
    stream: &mut S,
    data: &[u8],
) -> Result<()> {
    let len = u32::try_from(data.len())
        .map_err(|_| KeyzError::Backend("replication frame exceeds wire limit".into()))?
        .to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn read_replication_frame<S: AsyncRead + Unpin + ?Sized>(
    stream: &mut S,
) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    read_replication_payload(stream, len_buf).await
}

/// Live replication may be idle indefinitely between records. Once the first
/// byte arrives, the remaining header and payload share one frame deadline.
pub async fn read_live_replication_frame<S: AsyncRead + Unpin + ?Sized>(
    stream: &mut S,
    frame_timeout: Duration,
) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf[..1]).await?;
    tokio::time::timeout(frame_timeout, async {
        stream.read_exact(&mut len_buf[1..]).await?;
        read_replication_payload(stream, len_buf).await
    })
    .await
    .map_err(|_| KeyzError::ClientTimeout)?
}

async fn read_replication_payload<S: AsyncRead + Unpin + ?Sized>(
    stream: &mut S,
    len_buf: [u8; 4],
) -> Result<Vec<u8>> {
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_REPLICATION_FRAME {
        return Err(KeyzError::Backend(format!(
            "replication frame too large: {len} bytes (max {MAX_REPLICATION_FRAME})"
        )));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

pub fn is_sync_complete(frame: &[u8]) -> bool {
    frame.len() == 1 && frame[0] == SYNC_COMPLETE_MARKER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn live_frames_allow_idle_streams_between_complete_records() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let mut read = tokio::spawn(async move {
            read_live_replication_frame(&mut reader, Duration::from_millis(50)).await
        });
        assert!(tokio::time::timeout(Duration::from_millis(150), &mut read)
            .await
            .is_err());
        write_replication_frame(&mut writer, b"record")
            .await
            .unwrap();
        assert_eq!(read.await.unwrap().unwrap(), b"record");
    }

    #[tokio::test]
    async fn live_frame_deadline_covers_both_partial_headers_and_partial_payloads() {
        for prefix in [&[0][..], &[0, 0, 0, 100, 1][..]] {
            let (mut writer, mut reader) = tokio::io::duplex(128);
            writer.write_all(prefix).await.unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                read_live_replication_frame(&mut reader, Duration::from_millis(50)),
            )
            .await
            .unwrap();
            assert!(matches!(result, Err(KeyzError::ClientTimeout)));
        }
    }

    #[tokio::test]
    async fn live_frame_deadline_is_not_restarted_after_header_progress() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        writer.write_all(&[0]).await.unwrap();
        let read = tokio::spawn(async move {
            read_live_replication_frame(&mut reader, Duration::from_millis(150)).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        writer.write_all(&[0, 0, 3, b'a']).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), read)
                .await
                .unwrap()
                .unwrap(),
            Err(KeyzError::ClientTimeout)
        ));
    }
}
