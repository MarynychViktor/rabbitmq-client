use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use log::{info};
use tokio::io::{BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::protocol::types::{ChannelId, LongStr, Property, ShortStr, PropTable};
use crate::protocol::frame::{Frame, FrameEnvelope, ConnectionOpen, ConnectionStartOk, ConnectionTuneOk, ContentFrame, ConnectionClose};

use crate::{invoke_command_async, invoke_sync_method, Result, unwrap_frame_variant};
use crate::api::channel::AmqChannel;
use crate::api::connection::options::ConnectionArgs;
use crate::api::connection::constants::PROTOCOL_HEADER;
use crate::api::default_channel::DefaultAmqChannel;
use crate::building_blocks::{ChannelManager, Command, CommandPayload};
use self::constants::{COPYRIGHT, DEFAULT_AUTH_MECHANISM, DEFAULT_LOCALE, INFORMATION, PLATFORM, PRODUCT};
use crate::protocol::net::{FrameReader, FrameWriter};
use crate::utils::IdAllocator;

pub mod constants;
pub mod factory;
pub mod options;
pub use self::factory::ConnectionFactory;

pub struct Connection {
  arguments: ConnectionArgs,
  id_allocator: IdAllocator,
  message_tx: UnboundedSender<FrameEnvelope>,
  command_tx: UnboundedSender<Command>,
  close_tx: broadcast::Sender<()>,
}

impl Connection {
  pub async fn open(stream: TcpStream, args: ConnectionArgs) -> Result<Connection> {
    let stream_parts = stream.into_split();
    let mut reader = FrameReader::new(BufReader::new(stream_parts.0));
    let mut writer = FrameWriter::new(BufWriter::new(stream_parts.1));

    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (close_tx, close_rx) = broadcast::channel::<()>(1);

    let connection = Self {
      arguments: args,
      id_allocator: IdAllocator::new(),
      message_tx: msg_tx,
      command_tx,
      close_tx
    };

    connection.handshake(&mut reader, &mut writer).await?;
    connection.spawn_connection_handlers(reader, writer, msg_rx, command_rx);

    Ok(connection)
  }

  pub async fn create_channel(&mut self) -> Result<AmqChannel> {
    let id = self.id_allocator.allocate();
    info!("create channel");

    let (channel_tx, channel_rx) = mpsc::unbounded_channel();

    invoke_command_async!(self.command_tx, CommandPayload::RegisterChannel((id, channel_tx)));

    let channel = AmqChannel::open(id, self.message_tx.clone(), channel_rx, self.command_tx.clone()).await?;

    info!("channel created");
    Ok(channel)
  }

  pub async fn close(self) -> Result<()> {
    // todo!("provide reply code and text");
    let method = ConnectionClose {
      reply_code: 0,
      reply_text: "Connection closed".into(),
      class_id: 0,
      method_id: 0,
    };
    // invoke_sync_method!(0, self.command_tx, self.message_tx, method.into_frame()).await?;
    self.message_tx.send((0, method.into_frame())).unwrap();
    Ok(())
  }

  async fn handshake(&self, reader: &mut FrameReader, writer: &mut FrameWriter) -> Result<()> {
    info!("handshake started");
    writer.write_binary(&PROTOCOL_HEADER).await?;

    let (_, frame) = reader.next_frame().await?;
    let _start_method = unwrap_frame_variant!(frame, ConnectionStart);

    let client_properties: PropTable = HashMap::from([
      ("product".into(), Property::LongStr(PRODUCT.into())),
      ("platform".into(), Property::LongStr(PLATFORM.into())),
      ("copyright".into(), Property::LongStr(COPYRIGHT.into())),
      ("information".into(), Property::LongStr(INFORMATION.into()))
    ]);
    let start_ok_method = ConnectionStartOk {
      properties: client_properties,
      mechanism: ShortStr(DEFAULT_AUTH_MECHANISM.to_string()),
      response: LongStr(format!("\x00{}\x00{}", self.arguments.address.login.as_str(), self.arguments.address.password)),
      locale: ShortStr(DEFAULT_LOCALE.to_string()),
    };

    writer.dispatch(0, start_ok_method.into_frame()).await?;
    let (_, frame) = reader.next_frame().await?;
    let _tune_method = unwrap_frame_variant!(frame, ConnectionTune);

    let tune_ok_method = ConnectionTuneOk {
      chan_max: self.arguments.max_channels,
      frame_max: self.arguments.max_frame_size,
      heartbeat: self.arguments.heartbeat_interval
    };

    writer.dispatch(0, tune_ok_method.into_frame()).await?;

    let open_method = ConnectionOpen {
      vhost: self.arguments.address.vhost.clone().into(),
      reserved1: "".into(),
      reserved2: 0
    };

    writer.dispatch(0, open_method.into_frame()).await?;

    let (_, frame) = reader.next_frame().await?;
    let _open_ok_method = unwrap_frame_variant!(frame, ConnectionOpenOk);

    Ok(())
  }

  fn spawn_connection_handlers(
    &self,
    mut reader: FrameReader,
    mut writer: FrameWriter,
    mut outgoing_rx: UnboundedReceiver<FrameEnvelope>,
    mut command_rx: UnboundedReceiver<Command>
  ) {
    let mut channel_manager = ChannelManager::new();

    let (channel_tx, channel_rx) = mpsc::unbounded_channel();
    let default_channel = DefaultAmqChannel::open(
      self.message_tx.clone(),
      channel_rx,
      self.close_tx.clone()
    ).unwrap();
    channel_manager.register_channel(default_channel.id, channel_tx);

    let mut pending_frames: HashMap<ChannelId, ContentFrame> = HashMap::new();
    let heartbeat_interval = self.arguments.heartbeat_interval;
    let close_tx = self.close_tx.clone();
    let mut close_rx = self.close_tx.subscribe();

    let outgoing_tx = self.message_tx.clone();

    tokio::spawn(async move {
      let mut last_heartbeat = SystemTime::now();
      loop {
        let timeout_delay = tokio::time::sleep(Duration::from_secs(heartbeat_interval as u64));

        tokio::select! {
          Some((payload, acker)) = command_rx.recv() => {
            match payload {
              CommandPayload::RegisterResponder((channel, responder)) => {
                channel_manager.register_responder(channel, responder);
              },
              CommandPayload::RegisterChannel((id, incoming_tx)) => {
                channel_manager.register_channel(id, incoming_tx);
              },
              CommandPayload::RegisterConsumer(channel, consumer_tag, consumer_tx) => {
                channel_manager.register_consumer(channel, consumer_tag, consumer_tx);
              }
            }
            acker.send(()).unwrap();
          },
          Ok((channel, frame)) = reader.next_frame() => {
            last_heartbeat = SystemTime::now();

            match &frame {
              Frame::Heartbeat => {
                info!("Heartbeat received");
                // todo!("Do something with heartbeat");
              }
              Frame::ContentHeader(..) => {
                let pending_frame = pending_frames.remove(&channel).unwrap();
                let content_header = unwrap_frame_variant!(frame, ContentHeader);
                pending_frames.insert(channel, pending_frame.with_content_header(content_header));
              }
              Frame::ContentBody(..) => {
                let mut pending_frame = pending_frames.remove(&channel).unwrap();
                let content_body = unwrap_frame_variant!(frame, ContentBody);
                pending_frame = pending_frame.with_body(content_body);

                if pending_frame.is_complete() {
                  channel_manager.dispatch_content_frame(channel, outgoing_tx.clone(), pending_frame);
                } else {
                  pending_frames.insert(channel, pending_frame);
                }
              }
              Frame::ChannelOpenOk(..) |
              Frame::ExchangeDeclareOk(..) |
              Frame::QueueDeclareOk(..) |
              Frame::QueueBindOk(..) |
              Frame::QueueUnbindOk(..) |
              Frame::BasicConsumeOk(..) => {
                channel_manager.get_responder(channel).send(frame).unwrap();
              }
              Frame::BasicDeliver(..) => {
                pending_frames.insert(channel, ContentFrame::WithMethod(frame));
              }
              _ => {
                if channel == 0 {
                  channel_manager.dispatch_channel_frame((channel, frame)).unwrap();
                } else {
                  todo!("handle frame {:?}", frame);
                }
              }
            }
          },
          _ = timeout_delay => {
            if SystemTime::now().duration_since(last_heartbeat).unwrap().as_secs() >  heartbeat_interval as u64  * 2 {
              println!("Missing heartbeat");
              close_tx.send(()).unwrap();
            }
          },
          _ = close_rx.recv() => {
            break;
          }
        }
      }
      info!("exit reader loop");
    });

    let mut close_rx = self.close_tx.subscribe();
    tokio::spawn(async move {
      loop {
        let heartbeat_delay = tokio::time::sleep(Duration::from_secs(heartbeat_interval as u64));

        tokio::select! {
          Some((channel, frame)) = outgoing_rx.recv() => {
            writer.dispatch(channel, frame).await.unwrap();
          },
          _ = heartbeat_delay => {
            info!("heartbeat delivered");
            writer.dispatch(0, Frame::Heartbeat).await.unwrap();
          },
          _ = close_rx.recv() => {
            break;
          }
        };
      }

      info!("exit writer loop");
    });
  }
}
