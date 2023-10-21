use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Write};

use anyhow::{bail, Context, Result};
use crc32fast::Hasher;
use miniz_oxide::deflate::compress_to_vec_zlib;
use miniz_oxide::inflate::decompress_to_vec_zlib;
use thiserror::Error;

const USAGE: &str = "Usage: FILE... OUTPUT COMPRESSION";

const PNG_SIGNATURE: &[u8] = b"\x89PNG\x0D\x0A\x1A\x0A";
const PNG_CHUNK_IHDR_TYPE: &[u8] = b"IHDR";
const PNG_CHUNK_IDAT_TYPE: &[u8] = b"IDAT";
const PNG_CHUNK_IEND_TYPE: &[u8] = b"IEND";
const PNG_CHUNK_IHDR_LEN: usize = 13;

#[derive(Debug, PartialEq, Clone, Copy)]
enum PngChunkType {
    Ihdr,
    Idat,
    Iend,
}

struct PngChunk {
    type_code: PngChunkType,
    data: Box<[u8]>,
}

struct IhdrData {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    compression: u8,
    filter: u8,
    interlace: u8,
}

#[derive(Error, Debug)]
enum PngChunkError {
    #[error("IO error {source}")]
    Io {
        #[from]
        source: io::Error,
    },
    #[error("unsupported chunk type {0:?}")]
    UnsupportedType([u8; 4]),
    #[error("expected a chunk type of Ihdr, got {0:?}")]
    InvalidIhdrType(PngChunkType),
    #[error("expected a chunk length of {}, got {0}", PNG_CHUNK_IHDR_LEN)]
    InvalidIhdrLength(usize),
}

impl PngChunk {
    fn from_read(reader: &mut impl Read) -> Result<Self, PngChunkError> {
        let mut buf = [0u8; 4];

        reader.read_exact(&mut buf)?;
        let length = u32::from_be_bytes(buf) as usize;

        reader.read_exact(&mut buf)?;
        let type_code: PngChunkType = match buf.as_slice() {
            PNG_CHUNK_IHDR_TYPE => PngChunkType::Ihdr,
            PNG_CHUNK_IDAT_TYPE => PngChunkType::Idat,
            PNG_CHUNK_IEND_TYPE => PngChunkType::Iend,
            _ => {
                return Err(PngChunkError::UnsupportedType(buf));
            }
        };

        let mut data = Vec::with_capacity(length);
        if reader.take(length as u64).read_to_end(&mut data)? != length {
            return Err(PngChunkError::Io {
                source: io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "actual chunk data length < chunk length",
                ),
            });
        }

        // Skip over CRC
        reader.read_exact(&mut buf)?;

        Ok(Self {
            type_code,
            data: data.into_boxed_slice(),
        })
    }

    fn verify_ihdr(&self) -> Result<(), PngChunkError> {
        if self.type_code != PngChunkType::Ihdr {
            return Err(PngChunkError::InvalidIhdrType(self.type_code));
        }

        if self.data.len() != PNG_CHUNK_IHDR_LEN {
            return Err(PngChunkError::InvalidIhdrLength(self.data.len()));
        }

        Ok(())
    }

    fn to_ihdr_data(&self) -> Result<IhdrData> {
        self.verify_ihdr()?;

        let mut buf = [0u8; 4];

        buf.copy_from_slice(&self.data[0..4]);
        let width = u32::from_be_bytes(buf);

        buf.copy_from_slice(&self.data[4..8]);
        let height = u32::from_be_bytes(buf);

        let bit_depth = self.data[8];
        let color_type = self.data[9];
        let compression = self.data[10];
        let filter = self.data[11];
        let interlace = self.data[12];

        Ok(IhdrData {
            width,
            height,
            bit_depth,
            color_type,
            compression,
            filter,
            interlace,
        })
    }

    fn iend() -> Self {
        Self {
            type_code: PngChunkType::Iend,
            data: Box::new([]),
        }
    }

    fn write(&self, writer: &mut impl Write) -> Result<(), PngChunkError> {
        writer.write_all(
            &u32::try_from(self.data.len())
                .expect("u32 chunk length")
                .to_be_bytes(),
        )?;

        let type_code = match self.type_code {
            PngChunkType::Ihdr => PNG_CHUNK_IHDR_TYPE,
            PngChunkType::Idat => PNG_CHUNK_IDAT_TYPE,
            PngChunkType::Iend => PNG_CHUNK_IEND_TYPE,
        };
        writer.write_all(type_code)?;

        writer.write_all(&self.data)?;

        let mut hasher = Hasher::new();
        hasher.update(type_code);
        hasher.update(&self.data);
        writer.write_all(&(hasher.finalize().to_be_bytes()))?;

        Ok(())
    }
}

impl IhdrData {
    fn to_chunk(&self) -> PngChunk {
        let type_code = PngChunkType::Ihdr;
        let mut data = Vec::with_capacity(PNG_CHUNK_IHDR_LEN);
        data.extend_from_slice(&self.width.to_be_bytes());
        data.extend_from_slice(&self.height.to_be_bytes());
        data.push(self.bit_depth);
        data.push(self.color_type);
        data.push(self.compression);
        data.push(self.filter);
        data.push(self.interlace);

        PngChunk {
            type_code,
            data: data.into_boxed_slice(),
        }
    }
}

fn process_png(path: &str, saved: &mut Option<IhdrData>, infl_buf: &mut Vec<u8>) -> Result<()> {
    let file = File::open(path)?;
    let mut file = BufReader::new(file);

    let mut signature_buf = [0u8; PNG_SIGNATURE.len()];
    if file.read_exact(&mut signature_buf).is_err() || signature_buf != PNG_SIGNATURE {
        bail!("Invalid PNG signature");
    }

    let chunk = PngChunk::from_read(&mut file)?;
    let ihdr = chunk.to_ihdr_data()?;

    if let Some(x) = saved {
        if ihdr.width != x.width {
            bail!("Width does not match width of first PNG");
        }
    }

    let chunk = PngChunk::from_read(&mut file)?;
    infl_buf.append(&mut decompress_to_vec_zlib(&chunk.data)?);

    match saved {
        Some(x) => x.height += ihdr.height,
        None => *saved = Some(ihdr),
    };

    Ok(())
}

fn write_out(path: &str, saved: &IhdrData, infl_buf: &mut [u8], level: u8) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;

    file.write_all(PNG_SIGNATURE)?;
    saved.to_chunk().write(&mut file)?;

    PngChunk {
        type_code: PngChunkType::Idat,
        data: compress_to_vec_zlib(infl_buf, level).into_boxed_slice(),
    }
    .write(&mut file)?;
    PngChunk::iend().write(&mut file)?;

    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut saved: Option<IhdrData> = None;
    let mut infl_buf = Vec::new();

    if args.len() < 3 {
        bail!(USAGE);
    }

    let level: u8 = args[args.len() - 1]
        .trim()
        .parse()
        .context("Failed to parse compression level (should be 0-10)")?;

    for path in &args[..args.len() - 2] {
        if let Err(why) = process_png(path, &mut saved, &mut infl_buf) {
            eprintln!("Skipping \"{path}\": {why}");
        }
    }

    let file = args[args.len() - 2].as_str();
    match saved {
        None => bail!("Failed to process at least one PNG"),
        Some(x) => {
            write_out(file, &x, &mut infl_buf, level).context("Failed to write output PNG")?;
        }
    };

    Ok(())
}
