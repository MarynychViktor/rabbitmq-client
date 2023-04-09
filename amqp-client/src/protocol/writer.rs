use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::tcp::{OwnedWriteHalf};
use crate::protocol::types::{AmqpMethodArgs, Frame};
use crate::{Result};
use crate::protocol::frame2::{RawFrame};
use crate::protocol::enc::Encode;

pub struct FrameWriter {
  inner: BufWriter<OwnedWriteHalf>
}

impl FrameWriter {
  pub fn new(inner: BufWriter<OwnedWriteHalf>) -> Self {
    Self { inner }
  }

  pub async fn send_frame(&mut self, channel: i16, frame: Frame) -> Result<()> {
    let mut payload = frame.to_raw_repr();
    let mut frame_buff = vec![];

    frame_buff.write_byte(1).unwrap();
    frame_buff.write_short(channel).unwrap();
    frame_buff.write_uint(payload.len() as u32).unwrap();
    frame_buff.append(&mut payload);
    frame_buff.write_byte(0xCE).unwrap();

    self.write_binary(&frame_buff).await?;

    Ok(())
  }

  pub async fn write_binary<'a>(&'a mut self, buf: &'a [u8]) -> Result<()> {
    self.inner.write_all(buf).await?;
    self.inner.flush().await?;
    Ok(())
  }
}
