#![no_std]

//! A library for community with [HLK-LD6002](https://www.hlktech.net/index.php?id=1180) radar respiratory and heartbeat sensors.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use embedded_io_adapters::std::FromStd;
//! use hlk_ld6002::{Data, MessageStream};
//! use serialport::ClearBuffer;
//! use std::time::Duration;
//!
//! let port = serialport::new("/dev/ttyUSB0", 1_382_400)
//!     .timeout(Duration::from_millis(50))
//!     .open()
//!     .expect("Failed to open port");
//!
//! let mut messages = MessageStream::new(FromStd::new(port));
//!
//! let mut data = Data::default();
//!
//! for message in messages.flatten() {
//!     data.update(message);
//!     println!("{data:?}");
//! }
//! ```

use bytemuck::{cast, cast_slice};
use embedded_io::{Error, Read, ReadExactError};
use embedded_io_async::Read as AsyncRead;
use num_enum::TryFromPrimitive;

/// Error type for reading data from the sensor
#[derive(Debug)]
pub enum LdError<E> {
    /// The message received from the sensor had an unknown message type
    InvalidMessageType(u16),
    /// The message received from the sensor had an invalid length for the message type
    InvalidDataLength {
        expected: u16,
        got: u16,
        ty: MessageType,
    },
    /// The message received from the sensor had an invalid checksum
    InvalidChecksum {
        ty: &'static str,
        got: u8,
        expected: u8,
    },
    /// The data read from the sensor didn't start as expected
    InvalidFrameStart(u8),
    /// Unexpected end of data
    Eof,
    /// Error while reading from the serial device
    Read(E),
}

impl<E> From<ReadExactError<E>> for LdError<E> {
    fn from(value: ReadExactError<E>) -> Self {
        match value {
            ReadExactError::UnexpectedEof => LdError::Eof,
            ReadExactError::Other(e) => LdError::Read(e),
        }
    }
}

/// Message type sent by the sensor
#[derive(Debug, Clone, Copy, TryFromPrimitive)]
#[repr(u16)]
pub enum MessageType {
    Phase = 0x0a13,
    Respiratory = 0x0a14,
    Heartbeat = 0x0a15,
    Distance = 0x0a16,
}

impl MessageType {
    pub fn read<R: Read>(mut reader: R) -> Result<Self, LdError<R::Error>> {
        let mut bytes = [0u8; 2];
        reader.read_exact(&mut bytes)?;
        let ty = u16::from_be_bytes(bytes);
        MessageType::try_from(ty).map_err(|e| LdError::InvalidMessageType(e.number))
    }
}

impl MessageType {
    fn expected_length(self) -> u16 {
        match self {
            MessageType::Phase => 12,
            MessageType::Respiratory => 4,
            MessageType::Heartbeat => 4,
            MessageType::Distance => 8,
        }
    }
}

/// based on TinyFrame
#[derive(Clone, Debug)]
struct FrameHeader {
    _id: u16,
    length: u16,
    ty: MessageType,
}

impl FrameHeader {
    pub fn parse<E>(data: [u8; 7]) -> Result<Self, LdError<E>> {
        let ty = u16::from_be_bytes([data[4], data[5]]);
        let ty = MessageType::try_from(ty).map_err(|e| LdError::InvalidMessageType(e.number))?;

        // let checksum = checksum(&header_bytes);
        // if header_bytes[6] != checksum {
        //     return Err(LdError::InvalidChecksum {
        //         ty: "header",
        //         got: checksum,
        //         expected: header_bytes[6],
        //     });
        // }

        Ok(FrameHeader {
            _id: u16::from_be_bytes([data[0], data[1]]),
            length: u16::from_be_bytes([data[2], data[3]]),
            ty,
        })
    }

    pub fn read<R: Read>(mut reader: R) -> Result<Self, LdError<R::Error>> {
        let mut header_bytes = [0; 7];
        reader.read_exact(&mut header_bytes)?;

        Self::parse(header_bytes)
    }

    pub async fn read_async<R: AsyncRead>(mut reader: R) -> Result<Self, LdError<R::Error>> {
        let mut header_bytes = [0; 7];
        reader.read_exact(&mut header_bytes).await?;

        Self::parse(header_bytes)
    }
}

/// A frame of data received from the sensor
#[derive(Clone, Debug)]
#[allow(dead_code)]
struct Frame {
    header: FrameHeader,
    data: FrameData<16>,
}

impl Frame {
    pub fn read<R: Read>(mut reader: R) -> Result<Self, LdError<R::Error>> {
        let mut magic = [0];
        reader.read_exact(&mut magic)?;
        if magic[0] != 1 {
            return Err(LdError::InvalidFrameStart(magic[0]));
        }

        let header = FrameHeader::read(&mut reader)?;
        let data = FrameData::read(&mut reader, &header)?;
        let mut data_checksum = [0];
        reader.read_exact(&mut data_checksum)?;
        let data_checksum = data_checksum[0];

        let calculated_checksum = checksum(data.as_ref());
        if data_checksum != calculated_checksum {
            return Err(LdError::InvalidChecksum {
                ty: "body",
                got: calculated_checksum,
                expected: data_checksum,
            });
        };

        Ok(Frame { header, data })
    }

    pub async fn read_async<R: AsyncRead>(mut reader: R) -> Result<Self, LdError<R::Error>> {
        let mut magic = [0];
        reader.read_exact(&mut magic).await?;
        if magic[0] != 1 {
            return Err(LdError::InvalidFrameStart(magic[0]));
        }

        let header = FrameHeader::read_async(&mut reader).await?;
        let data = FrameData::read_async(&mut reader, &header).await?;
        let mut data_checksum = [0];
        reader.read_exact(&mut data_checksum).await?;
        let data_checksum = data_checksum[0];

        let calculated_checksum = checksum(data.as_ref());
        if data_checksum != calculated_checksum {
            return Err(LdError::InvalidChecksum {
                ty: "body",
                got: calculated_checksum,
                expected: data_checksum,
            });
        };

        Ok(Frame { header, data })
    }
}

#[derive(Debug, Clone)]
struct FrameData<const N: usize> {
    _align: u32,
    data: [u8; N],
    len: u16,
}

impl<const N: usize> FrameData<N> {
    pub fn len(&self) -> u16 {
        self.len
    }

    fn validate<E>(header: &FrameHeader) -> Result<(), LdError<E>> {
        if header.length as usize > N || header.length != header.ty.expected_length() {
            Err(LdError::InvalidDataLength {
                got: header.length,
                expected: header.ty.expected_length(),
                ty: header.ty,
            })
        } else {
            Ok(())
        }
    }

    pub fn read<R: Read>(mut reader: R, header: &FrameHeader) -> Result<Self, LdError<R::Error>> {
        Self::validate(header)?;

        let mut data = [0u8; N];
        reader.read_exact(&mut data[0..header.length as usize])?;

        Ok(FrameData {
            _align: 0,
            data,
            len: header.length,
        })
    }

    pub async fn read_async<R: AsyncRead>(
        mut reader: R,
        header: &FrameHeader,
    ) -> Result<Self, LdError<R::Error>> {
        Self::validate(header)?;

        let mut data = [0u8; N];
        reader
            .read_exact(&mut data[0..header.length as usize])
            .await?;

        Ok(FrameData {
            _align: 0,
            data,
            len: header.length,
        })
    }
}

impl<const N: usize> AsRef<[u8]> for FrameData<N> {
    fn as_ref(&self) -> &[u8] {
        &self.data[0..self.len as usize]
    }
}

impl Frame {
    /// Decode the body of the message according to the message type
    fn body<E: Error>(&self) -> Result<MessageBody, LdError<E>> {
        let numbers = cast_slice::<_, u32>(self.data.as_ref());

        match (self.header.ty, self.data.len()) {
            (MessageType::Phase, 12) => {
                let numbers: [u32; 3] = numbers.try_into().unwrap();
                Ok(MessageBody::Phase(cast(numbers)))
            }
            (MessageType::Respiratory, 4) => {
                Ok(MessageBody::Respiratory(f32::from_bits(numbers[0])))
            }
            (MessageType::Heartbeat, 4) => Ok(MessageBody::Heartbeat(f32::from_bits(numbers[0]))),
            (MessageType::Distance, 8) => {
                let distance = if numbers[0] == 1 {
                    f32::from_bits(numbers[1])
                } else {
                    0.0
                };
                Ok(MessageBody::Distance(Some(distance)))
            }
            (MessageType::Distance, 4) => Ok(MessageBody::Distance(None)),
            _ => Err(LdError::InvalidDataLength {
                got: self.data.len(),
                expected: self.header.ty.expected_length(),
                ty: self.header.ty,
            }),
        }
    }
}

/// The decoded message from the sensor
#[derive(Clone, Debug)]
pub enum MessageBody {
    Phase([f32; 3]),
    Respiratory(f32),
    Heartbeat(f32),
    Distance(Option<f32>),
}

/// A wrapper around [`Read`](embedded-io::Read) for reading messages from the sensor
pub struct MessageStream<R> {
    reader: R,
}

impl<R: Read> MessageStream<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    fn read(&mut self) -> Result<Frame, LdError<R::Error>> {
        Frame::read(&mut self.reader)
    }
}

impl<R: Read> Iterator for MessageStream<R> {
    type Item = Result<MessageBody, LdError<R::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        let frame = match self.read() {
            Ok(frame) => frame,
            Err(e) => return Some(Err(e)),
        };

        Some(frame.body::<R::Error>())
    }
}

/// A wrapper around [`AsyncRead`](embedded-io-async::AsyncRead) for reading messages from the sensor
pub struct AsyncMessageStream<R> {
    reader: R,
}

impl<R: AsyncRead> AsyncMessageStream<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    async fn read(&mut self) -> Result<Frame, LdError<R::Error>> {
        Frame::read_async(&mut self.reader).await
    }

    /// Read the next message from the sensor
    pub async fn next(&mut self) -> Result<MessageBody, LdError<R::Error>> {
        let frame = self.read().await?;
        frame.body()
    }
}

/// A helper struct to store the received data
#[derive(Default, Debug, Copy, Clone)]
pub struct Data {
    pub respiratory: f32,
    pub distance: f32,
    pub heartbeat: f32,
}

impl Data {
    pub fn update(&mut self, message: MessageBody) {
        match message {
            MessageBody::Respiratory(rate) if rate > 0.0 => {
                self.respiratory = rate;
            }
            MessageBody::Distance(Some(distance)) if distance > 0.0 => {
                self.distance = distance;
            }
            MessageBody::Heartbeat(rate) if rate > 0.0 => {
                self.heartbeat = rate;
            }
            _ => {}
        }
    }
}

fn checksum(data: &[u8]) -> u8 {
    let mut result = 0;
    for byte in data {
        result ^= byte;
    }
    !result
}