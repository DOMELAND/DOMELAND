use crate::{
    metrics::NetworkMetrics,
    types::{Frame, Mid, Pid, Sid},
};
use async_std::{
    net::{TcpStream, UdpSocket},
    prelude::*,
    sync::RwLock,
};
use futures::{channel::mpsc, future::FutureExt, select, sink::SinkExt, stream::StreamExt};
use std::{net::SocketAddr, sync::Arc};
use tracing::*;

// Reserving bytes 0, 10, 13 as i have enough space and want to make it easy to
// detect a invalid client, e.g. sending an empty line would make 10 first char
// const FRAME_RESERVED_1: u8 = 0;
const FRAME_HANDSHAKE: u8 = 1;
const FRAME_PARTICIPANT_ID: u8 = 2;
const FRAME_SHUTDOWN: u8 = 3;
const FRAME_OPEN_STREAM: u8 = 4;
const FRAME_CLOSE_STREAM: u8 = 5;
const FRAME_DATA_HEADER: u8 = 6;
const FRAME_DATA: u8 = 7;
const FRAME_RAW: u8 = 8;
//const FRAME_RESERVED_2: u8 = 10;
//const FRAME_RESERVED_3: u8 = 13;

#[derive(Debug)]
pub(crate) enum Protocols {
    Tcp(TcpProtocol),
    Udp(UdpProtocol),
    //Mpsc(MpscChannel),
}

#[derive(Debug)]
pub(crate) struct TcpProtocol {
    stream: TcpStream,
    metrics: Arc<NetworkMetrics>,
}

#[derive(Debug)]
pub(crate) struct UdpProtocol {
    socket: Arc<UdpSocket>,
    remote_addr: SocketAddr,
    metrics: Arc<NetworkMetrics>,
    data_in: RwLock<mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl TcpProtocol {
    pub(crate) fn new(stream: TcpStream, metrics: Arc<NetworkMetrics>) -> Self {
        Self { stream, metrics }
    }

    pub async fn read(&self, mut frame_handler: mpsc::UnboundedSender<Frame>) {
        let mut stream = self.stream.clone();
        loop {
            let mut bytes = [0u8; 1];
            if stream.read_exact(&mut bytes).await.is_err() {
                info!("tcp channel closed, shutting down read");
                break;
            }
            let frame_no = bytes[0];
            let frame = match frame_no {
                FRAME_HANDSHAKE => {
                    let mut bytes = [0u8; 19];
                    stream.read_exact(&mut bytes).await.unwrap();
                    let magic_number = [
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                    ];
                    Frame::Handshake {
                        magic_number,
                        version: [
                            u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]),
                            u32::from_le_bytes([bytes[11], bytes[12], bytes[13], bytes[14]]),
                            u32::from_le_bytes([bytes[15], bytes[16], bytes[17], bytes[18]]),
                        ],
                    }
                },
                FRAME_PARTICIPANT_ID => {
                    let mut bytes = [0u8; 16];
                    stream.read_exact(&mut bytes).await.unwrap();
                    let pid = Pid::from_le_bytes(bytes);
                    Frame::ParticipantId { pid }
                },
                FRAME_SHUTDOWN => Frame::Shutdown,
                FRAME_OPEN_STREAM => {
                    let mut bytes = [0u8; 10];
                    stream.read_exact(&mut bytes).await.unwrap();
                    let sid = Sid::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]);
                    let prio = bytes[8];
                    let promises = bytes[9];
                    Frame::OpenStream {
                        sid,
                        prio,
                        promises,
                    }
                },
                FRAME_CLOSE_STREAM => {
                    let mut bytes = [0u8; 8];
                    stream.read_exact(&mut bytes).await.unwrap();
                    let sid = Sid::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]);
                    Frame::CloseStream { sid }
                },
                FRAME_DATA_HEADER => {
                    let mut bytes = [0u8; 24];
                    stream.read_exact(&mut bytes).await.unwrap();
                    let mid = Mid::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]);
                    let sid = Sid::from_le_bytes([
                        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                        bytes[15],
                    ]);
                    let length = u64::from_le_bytes([
                        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21],
                        bytes[22], bytes[23],
                    ]);
                    Frame::DataHeader { mid, sid, length }
                },
                FRAME_DATA => {
                    let mut bytes = [0u8; 18];
                    stream.read_exact(&mut bytes).await.unwrap();
                    let mid = Mid::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]);
                    let start = u64::from_le_bytes([
                        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                        bytes[15],
                    ]);
                    let length = u16::from_le_bytes([bytes[16], bytes[17]]);
                    let mut data = vec![0; length as usize];
                    stream.read_exact(&mut data).await.unwrap();
                    Frame::Data { mid, start, data }
                },
                FRAME_RAW => {
                    let mut bytes = [0u8; 2];
                    stream.read_exact(&mut bytes).await.unwrap();
                    let length = u16::from_le_bytes([bytes[0], bytes[1]]);
                    let mut data = vec![0; length as usize];
                    stream.read_exact(&mut data).await.unwrap();
                    Frame::Raw(data)
                },
                _ => {
                    // report a RAW frame, but cannot rely on the next 2 bytes to be a size.
                    // guessing 256 bytes, which might help to sort down issues
                    let mut data = vec![0; 256];
                    stream.read(&mut data).await.unwrap();
                    Frame::Raw(data)
                },
            };
            frame_handler.send(frame).await.unwrap();
        }
        trace!("shutting down tcp read()");
    }

    //dezerialize here as this is executed in a seperate thread PER channel.
    // Limites Throughput per single Receiver but stays in same thread (maybe as its
    // in a threadpool) for TCP, UDP and MPSC
    pub async fn write(
        &self,
        mut internal_frame_receiver: mpsc::UnboundedReceiver<Frame>,
        mut external_frame_receiver: mpsc::UnboundedReceiver<Frame>,
    ) {
        let mut stream = self.stream.clone();
        while let Some(frame) = select! {
            next = internal_frame_receiver.next().fuse() => next,
            next = external_frame_receiver.next().fuse() => next,
        } {
            match frame {
                Frame::Handshake {
                    magic_number,
                    version,
                } => {
                    stream
                        .write_all(&FRAME_HANDSHAKE.to_be_bytes())
                        .await
                        .unwrap();
                    stream.write_all(&magic_number).await.unwrap();
                    stream.write_all(&version[0].to_le_bytes()).await.unwrap();
                    stream.write_all(&version[1].to_le_bytes()).await.unwrap();
                    stream.write_all(&version[2].to_le_bytes()).await.unwrap();
                },
                Frame::ParticipantId { pid } => {
                    stream
                        .write_all(&FRAME_PARTICIPANT_ID.to_be_bytes())
                        .await
                        .unwrap();
                    stream.write_all(&pid.to_le_bytes()).await.unwrap();
                },
                Frame::Shutdown => {
                    stream
                        .write_all(&FRAME_SHUTDOWN.to_be_bytes())
                        .await
                        .unwrap();
                },
                Frame::OpenStream {
                    sid,
                    prio,
                    promises,
                } => {
                    stream
                        .write_all(&FRAME_OPEN_STREAM.to_be_bytes())
                        .await
                        .unwrap();
                    stream.write_all(&sid.to_le_bytes()).await.unwrap();
                    stream.write_all(&prio.to_le_bytes()).await.unwrap();
                    stream.write_all(&promises.to_le_bytes()).await.unwrap();
                },
                Frame::CloseStream { sid } => {
                    stream
                        .write_all(&FRAME_CLOSE_STREAM.to_be_bytes())
                        .await
                        .unwrap();
                    stream.write_all(&sid.to_le_bytes()).await.unwrap();
                },
                Frame::DataHeader { mid, sid, length } => {
                    stream
                        .write_all(&FRAME_DATA_HEADER.to_be_bytes())
                        .await
                        .unwrap();
                    stream.write_all(&mid.to_le_bytes()).await.unwrap();
                    stream.write_all(&sid.to_le_bytes()).await.unwrap();
                    stream.write_all(&length.to_le_bytes()).await.unwrap();
                },
                Frame::Data { mid, start, data } => {
                    stream.write_all(&FRAME_DATA.to_be_bytes()).await.unwrap();
                    stream.write_all(&mid.to_le_bytes()).await.unwrap();
                    stream.write_all(&start.to_le_bytes()).await.unwrap();
                    stream
                        .write_all(&(data.len() as u16).to_le_bytes())
                        .await
                        .unwrap();
                    stream.write_all(&data).await.unwrap();
                },
                Frame::Raw(data) => {
                    stream.write_all(&FRAME_RAW.to_be_bytes()).await.unwrap();
                    stream
                        .write_all(&(data.len() as u16).to_le_bytes())
                        .await
                        .unwrap();
                    stream.write_all(&data).await.unwrap();
                },
            }
        }
        trace!("shutting down tcp write()");
    }
}

impl UdpProtocol {
    pub(crate) fn new(
        socket: Arc<UdpSocket>,
        remote_addr: SocketAddr,
        metrics: Arc<NetworkMetrics>,
        data_in: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Self {
        Self {
            socket,
            remote_addr,
            metrics,
            data_in: RwLock::new(data_in),
        }
    }

    pub async fn read(&self, mut frame_handler: mpsc::UnboundedSender<Frame>) {
        let mut data_in = self.data_in.write().await;
        while let Some(bytes) = data_in.next().await {
            trace!("got raw UDP message with len: {}", bytes.len());
            let frame_no = bytes[0];
            let frame = match frame_no {
                FRAME_HANDSHAKE => {
                    let bytes = &bytes[1..20];
                    let magic_number = [
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                    ];
                    Frame::Handshake {
                        magic_number,
                        version: [
                            u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]),
                            u32::from_le_bytes([bytes[11], bytes[12], bytes[13], bytes[14]]),
                            u32::from_le_bytes([bytes[15], bytes[16], bytes[17], bytes[18]]),
                        ],
                    }
                },
                FRAME_PARTICIPANT_ID => {
                    let pid = Pid::from_le_bytes([
                        bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                        bytes[15], bytes[16],
                    ]);
                    Frame::ParticipantId { pid }
                },
                FRAME_SHUTDOWN => Frame::Shutdown,
                FRAME_OPEN_STREAM => {
                    let bytes = &bytes[1..11];
                    let sid = Sid::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]);
                    let prio = bytes[8];
                    let promises = bytes[9];
                    Frame::OpenStream {
                        sid,
                        prio,
                        promises,
                    }
                },
                FRAME_CLOSE_STREAM => {
                    let bytes = &bytes[1..9];
                    let sid = Sid::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]);
                    Frame::CloseStream { sid }
                },
                FRAME_DATA_HEADER => {
                    let bytes = &bytes[1..25];
                    let mid = Mid::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]);
                    let sid = Sid::from_le_bytes([
                        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                        bytes[15],
                    ]);
                    let length = u64::from_le_bytes([
                        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21],
                        bytes[22], bytes[23],
                    ]);
                    Frame::DataHeader { mid, sid, length }
                },
                FRAME_DATA => {
                    let mid = Mid::from_le_bytes([
                        bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                        bytes[8],
                    ]);
                    let start = u64::from_le_bytes([
                        bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
                        bytes[16],
                    ]);
                    let length = u16::from_le_bytes([bytes[17], bytes[18]]);
                    let mut data = vec![0; length as usize];
                    data.copy_from_slice(&bytes[19..]);
                    Frame::Data { mid, start, data }
                },
                FRAME_RAW => {
                    error!("Uffff");
                    let length = u16::from_le_bytes([bytes[1], bytes[2]]);
                    let mut data = vec![0; length as usize];
                    data.copy_from_slice(&bytes[3..]);
                    Frame::Raw(data)
                },
                _ => Frame::Raw(bytes),
            };
            frame_handler.send(frame).await.unwrap();
        }
        /*
        let mut data_in = self.data_in.write().await;
        let mut buffer = NetworkBuffer::new();
        while let Some(data) = data_in.next().await {
            let n = data.len();
            let slice = &mut buffer.get_write_slice(n)[0..n]; //get_write_slice can return  more then n!
            slice.clone_from_slice(data.as_slice());
            buffer.actually_written(n);
            trace!("incomming message with len: {}", n);
            let slice = buffer.get_read_slice();
            let mut cur = std::io::Cursor::new(slice);
            let mut read_ok = 0;
            while cur.position() < n as u64 {
                let round_start = cur.position() as usize;
                let r: Result<Frame, _> = bincode::deserialize_from(&mut cur);
                match r {
                    Ok(frame) => {
                        frame_handler.send(frame).await.unwrap();
                        read_ok = cur.position() as usize;
                    },
                    Err(e) => {
                        // Probably we have to wait for moare data!
                        let first_bytes_of_msg =
                            &slice[round_start..std::cmp::min(n, round_start + 16)];
                        debug!(
                            ?buffer,
                            ?e,
                            ?n,
                            ?round_start,
                            ?first_bytes_of_msg,
                            "message cant be parsed, probably because we need to wait for more \
                             data"
                        );
                        break;
                    },
                }
            }
            buffer.actually_read(read_ok);
        }*/
        trace!("shutting down udp read()");
    }

    pub async fn write(
        &self,
        mut internal_frame_receiver: mpsc::UnboundedReceiver<Frame>,
        mut external_frame_receiver: mpsc::UnboundedReceiver<Frame>,
    ) {
        let mut buffer = [0u8; 2000];
        while let Some(frame) = select! {
            next = internal_frame_receiver.next().fuse() => next,
            next = external_frame_receiver.next().fuse() => next,
        } {
            let len = match frame {
                Frame::Handshake {
                    magic_number,
                    version,
                } => {
                    let x = FRAME_HANDSHAKE.to_be_bytes();
                    buffer[0] = x[0];
                    buffer[1] = magic_number[0];
                    buffer[2] = magic_number[1];
                    buffer[3] = magic_number[2];
                    buffer[4] = magic_number[3];
                    buffer[5] = magic_number[4];
                    buffer[6] = magic_number[5];
                    buffer[7] = magic_number[6];
                    let x = version[0].to_le_bytes();
                    buffer[8] = x[0];
                    buffer[9] = x[1];
                    buffer[10] = x[2];
                    buffer[11] = x[3];
                    let x = version[1].to_le_bytes();
                    buffer[12] = x[0];
                    buffer[13] = x[1];
                    buffer[14] = x[2];
                    buffer[15] = x[3];
                    let x = version[2].to_le_bytes();
                    buffer[16] = x[0];
                    buffer[17] = x[1];
                    buffer[18] = x[2];
                    buffer[19] = x[3];
                    20
                },
                Frame::ParticipantId { pid } => {
                    let x = FRAME_PARTICIPANT_ID.to_be_bytes();
                    buffer[0] = x[0];
                    let x = pid.to_le_bytes();
                    buffer[1] = x[0];
                    buffer[2] = x[1];
                    buffer[3] = x[2];
                    buffer[4] = x[3];
                    buffer[5] = x[4];
                    buffer[6] = x[5];
                    buffer[7] = x[6];
                    buffer[8] = x[7];
                    buffer[9] = x[8];
                    buffer[10] = x[9];
                    buffer[11] = x[10];
                    buffer[12] = x[11];
                    buffer[13] = x[12];
                    buffer[14] = x[13];
                    buffer[15] = x[14];
                    buffer[16] = x[15];
                    17
                },
                Frame::Shutdown => {
                    let x = FRAME_SHUTDOWN.to_be_bytes();
                    buffer[0] = x[0];
                    1
                },
                Frame::OpenStream {
                    sid,
                    prio,
                    promises,
                } => {
                    let x = FRAME_OPEN_STREAM.to_be_bytes();
                    buffer[0] = x[0];
                    let x = sid.to_le_bytes();
                    buffer[1] = x[0];
                    buffer[2] = x[1];
                    buffer[3] = x[2];
                    buffer[4] = x[3];
                    buffer[5] = x[4];
                    buffer[6] = x[5];
                    buffer[7] = x[6];
                    buffer[8] = x[7];
                    let x = prio.to_le_bytes();
                    buffer[9] = x[0];
                    let x = promises.to_le_bytes();
                    buffer[10] = x[0];
                    11
                },
                Frame::CloseStream { sid } => {
                    let x = FRAME_CLOSE_STREAM.to_be_bytes();
                    buffer[0] = x[0];
                    let x = sid.to_le_bytes();
                    buffer[1] = x[0];
                    buffer[2] = x[1];
                    buffer[3] = x[2];
                    buffer[4] = x[3];
                    buffer[5] = x[4];
                    buffer[6] = x[5];
                    buffer[7] = x[6];
                    buffer[8] = x[7];
                    9
                },
                Frame::DataHeader { mid, sid, length } => {
                    let x = FRAME_DATA_HEADER.to_be_bytes();
                    buffer[0] = x[0];
                    let x = mid.to_le_bytes();
                    buffer[1] = x[0];
                    buffer[2] = x[1];
                    buffer[3] = x[2];
                    buffer[4] = x[3];
                    buffer[5] = x[4];
                    buffer[6] = x[5];
                    buffer[7] = x[6];
                    buffer[8] = x[7];
                    let x = sid.to_le_bytes();
                    buffer[9] = x[0];
                    buffer[10] = x[1];
                    buffer[11] = x[2];
                    buffer[12] = x[3];
                    buffer[13] = x[4];
                    buffer[14] = x[5];
                    buffer[15] = x[6];
                    buffer[16] = x[7];
                    let x = length.to_le_bytes();
                    buffer[17] = x[0];
                    buffer[18] = x[1];
                    buffer[19] = x[2];
                    buffer[20] = x[3];
                    buffer[21] = x[4];
                    buffer[22] = x[5];
                    buffer[23] = x[6];
                    buffer[24] = x[7];
                    25
                },
                Frame::Data { mid, start, data } => {
                    let x = FRAME_DATA.to_be_bytes();
                    buffer[0] = x[0];
                    let x = mid.to_le_bytes();
                    buffer[1] = x[0];
                    buffer[2] = x[1];
                    buffer[3] = x[2];
                    buffer[4] = x[3];
                    buffer[5] = x[4];
                    buffer[6] = x[5];
                    buffer[7] = x[6];
                    buffer[8] = x[7];
                    let x = start.to_le_bytes();
                    buffer[9] = x[0];
                    buffer[10] = x[1];
                    buffer[11] = x[2];
                    buffer[12] = x[3];
                    buffer[13] = x[4];
                    buffer[14] = x[5];
                    buffer[15] = x[6];
                    buffer[16] = x[7];
                    let x = (data.len() as u16).to_le_bytes();
                    buffer[17] = x[0];
                    buffer[18] = x[1];
                    for i in 0..data.len() {
                        buffer[19 + i] = data[i];
                    }
                    19 + data.len()
                },
                Frame::Raw(data) => {
                    let x = FRAME_RAW.to_be_bytes();
                    buffer[0] = x[0];
                    let x = (data.len() as u16).to_le_bytes();
                    buffer[1] = x[0];
                    buffer[2] = x[1];
                    for i in 0..data.len() {
                        buffer[3 + i] = data[i];
                    }
                    3 + data.len()
                },
            };
            let mut start = 0;
            while start < len {
                trace!(?start, ?len, "splitting up udp frame in multiple packages");
                match self
                    .socket
                    .send_to(&buffer[start..len], self.remote_addr)
                    .await
                {
                    Ok(n) => {
                        start += n;
                        if n != len {
                            error!(
                                "THIS DOESNT WORK, as RECEIVER CURRENLTY ONLY HANDLES 1 FRAME per \
                                 UDP message. splitting up will fail!"
                            );
                        }
                    },
                    Err(e) => error!(?e, "need to handle that error!"),
                }
            }
        }
        trace!("shutting down udp write()");
        /*
        let mut buffer = NetworkBuffer::new();
        while let Some(frame) = select! {
            next = internal_frame_receiver.next().fuse() => next,
            next = external_frame_receiver.next().fuse() => next,
        } {
            let len = bincode::serialized_size(&frame).unwrap() as usize;
            match bincode::serialize_into(buffer.get_write_slice(len), &frame) {
                Ok(_) => buffer.actually_written(len),
                Err(e) => error!("Oh nooo {}", e),
            };
            trace!(?len, "going to send frame via Udp");
            let mut to_send = buffer.get_read_slice();
            while to_send.len() > 0 {
                match self.socket.send_to(to_send, self.remote_addr).await {
                    Ok(n) => buffer.actually_read(n),
                    Err(e) => error!(?e, "need to handle that error!"),
                }
                to_send = buffer.get_read_slice();
            }
        }
        */
    }
}

// INTERNAL NetworkBuffer
/*
struct NetworkBuffer {
    pub(crate) data: Vec<u8>,
    pub(crate) read_idx: usize,
    pub(crate) write_idx: usize,
}

/// NetworkBuffer to use for streamed access
/// valid data is between read_idx and write_idx!
/// everything before read_idx is already processed and no longer important
/// everything after write_idx is either 0 or random data buffered
impl NetworkBuffer {
    fn new() -> Self {
        NetworkBuffer {
            data: vec![0; 2048],
            read_idx: 0,
            write_idx: 0,
        }
    }

    fn get_write_slice(&mut self, min_size: usize) -> &mut [u8] {
        if self.data.len() < self.write_idx + min_size {
            trace!(
                ?self,
                ?min_size,
                "need to resize because buffer is to small"
            );
            self.data.resize(self.write_idx + min_size, 0);
        }
        &mut self.data[self.write_idx..]
    }

    fn actually_written(&mut self, cnt: usize) { self.write_idx += cnt; }

    fn get_read_slice(&self) -> &[u8] { &self.data[self.read_idx..self.write_idx] }

    fn actually_read(&mut self, cnt: usize) {
        self.read_idx += cnt;
        if self.read_idx == self.write_idx {
            if self.read_idx > 10485760 {
                trace!(?self, "buffer empty, resetting indices");
            }
            self.read_idx = 0;
            self.write_idx = 0;
        }
        if self.write_idx > 10485760 {
            if self.write_idx - self.read_idx < 65536 {
                debug!(
                    ?self,
                    "This buffer is filled over 10 MB, but the actual data diff is less then \
                     65kB, which is a sign of stressing this connection much as always new data \
                     comes in - nevertheless, in order to handle this we will remove some data \
                     now so that this buffer doesn't grow endlessly"
                );
                let mut i2 = 0;
                for i in self.read_idx..self.write_idx {
                    self.data[i2] = self.data[i];
                    i2 += 1;
                }
                self.read_idx = 0;
                self.write_idx = i2;
            }
            if self.data.len() > 67108864 {
                warn!(
                    ?self,
                    "over 64Mbyte used, something seems fishy, len: {}",
                    self.data.len()
                );
            }
        }
    }
}

impl std::fmt::Debug for NetworkBuffer {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "NetworkBuffer(len: {}, read: {}, write: {})",
            self.data.len(),
            self.read_idx,
            self.write_idx
        )
    }
}

*/
