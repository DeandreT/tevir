use bytes::{BufMut as _, BytesMut};
use protocol::{CodecError, Envelope, FrameCodec};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

const LENGTH_PREFIX_BYTES: usize = size_of::<u32>();

pub(crate) async fn write_control<W>(
    writer: &mut W,
    message: &Envelope,
    maximum: usize,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let frame = FrameCodec::new(maximum)?.encode(message)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

pub(crate) async fn read_control<R>(reader: &mut R, maximum: usize) -> Result<Envelope, FrameError>
where
    R: AsyncRead + Unpin,
{
    let payload_length = reader.read_u32().await? as usize;
    if payload_length > maximum {
        return Err(FrameError::FrameTooLarge {
            actual: payload_length,
            maximum,
        });
    }
    let mut payload = vec![0; payload_length];
    reader.read_exact(&mut payload).await?;

    let mut frame = BytesMut::with_capacity(LENGTH_PREFIX_BYTES + payload_length);
    frame.put_u32(
        u32::try_from(payload_length).map_err(|_| FrameError::FrameTooLarge {
            actual: payload_length,
            maximum,
        })?,
    );
    frame.extend_from_slice(&payload);
    FrameCodec::new(maximum)?
        .decode(&mut frame)?
        .ok_or(FrameError::IncompleteFrame)
}

pub(crate) async fn write_bulk<W>(
    writer: &mut W,
    payload: &[u8],
    maximum: usize,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > maximum {
        return Err(FrameError::FrameTooLarge {
            actual: payload.len(),
            maximum,
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::FrameTooLarge {
        actual: payload.len(),
        maximum,
    })?;
    writer.write_u32(length).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

pub(crate) async fn read_bulk<R>(reader: &mut R, maximum: usize) -> Result<Vec<u8>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let payload_length = reader.read_u32().await? as usize;
    if payload_length > maximum {
        return Err(FrameError::FrameTooLarge {
            actual: payload_length,
            maximum,
        });
    }
    let mut payload = vec![0; payload_length];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("stream I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("frame is {actual} bytes; the configured maximum is {maximum}")]
    FrameTooLarge { actual: usize, maximum: usize },
    #[error("framing completed without a message")]
    IncompleteFrame,
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt as _, duplex};

    use super::{FrameError, read_bulk};

    #[tokio::test]
    async fn rejects_an_oversized_frame_before_reading_its_payload() {
        let (mut writer, mut reader) = duplex(64);
        writer
            .write_u32(33)
            .await
            .unwrap_or_else(|error| panic!("prefix write failed: {error}"));

        assert!(matches!(
            read_bulk(&mut reader, 32).await,
            Err(FrameError::FrameTooLarge {
                actual: 33,
                maximum: 32
            })
        ));
    }

    #[tokio::test]
    async fn reports_connection_loss_during_a_frame() {
        let (mut writer, mut reader) = duplex(64);
        writer
            .write_u32(8)
            .await
            .unwrap_or_else(|error| panic!("prefix write failed: {error}"));
        writer
            .write_all(&[1, 2])
            .await
            .unwrap_or_else(|error| panic!("payload write failed: {error}"));
        drop(writer);

        assert!(matches!(
            read_bulk(&mut reader, 32).await,
            Err(FrameError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
    }
}
