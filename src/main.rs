use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Cursor, Read, Seek, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use crc32fast::Hasher;
use miniz_oxide::deflate::compress_to_vec_zlib;
use miniz_oxide::inflate::{decompress_to_vec_zlib, DecompressError};
use thiserror::Error;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\x0D\x0A\x1A\x0A";

#[derive(Error, Debug)]
enum PngError {
    #[error("IO error")]
    Io(#[from] io::Error),
    #[error("Decompress error")]
    Decompress(#[from] DecompressError),
    #[error("invalid PNG file signature")]
    InvalidSignature,
    #[error("unsupported chunk type code {0:?}")]
    UnsupportedTypeCode([u8; 4]),
    #[error("invalid IHDR length")]
    InvalidIhdrLength,
    #[error("chunk type code is not IHDR")]
    NotIhdr,
    #[error("png width is not equal to the first's")]
    UnequalWidth,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum PngChunkKind {
    Ihdr,
    Idat,
    Iend,
}

impl PngChunkKind {
    const IHDR: &'static [u8; 4] = b"IHDR";
    const IDAT: &'static [u8; 4] = b"IDAT";
    const IEND: &'static [u8; 4] = b"IEND";
}

impl TryFrom<&[u8; 4]> for PngChunkKind {
    type Error = PngError;
    fn try_from(type_code: &[u8; 4]) -> Result<Self, Self::Error> {
        use PngChunkKind::*;
        match type_code {
            PngChunkKind::IHDR => Ok(Ihdr),
            PngChunkKind::IDAT => Ok(Idat),
            PngChunkKind::IEND => Ok(Iend),
            _ => Err(PngError::UnsupportedTypeCode(*type_code)),
        }
    }
}

impl From<PngChunkKind> for &[u8; 4] {
    fn from(kind: PngChunkKind) -> Self {
        use PngChunkKind::*;
        match kind {
            Ihdr => PngChunkKind::IHDR,
            Idat => PngChunkKind::IDAT,
            Iend => PngChunkKind::IEND,
        }
    }
}

#[derive(Debug)]
struct PngChunk {
    kind: PngChunkKind,
    data: Box<[u8]>,
}

trait ReadExactExt: Read {
    #[inline]
    fn read_exact_capacity(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        self.take(buf.capacity() as u64)
            .read_to_end(buf)
            .and_then(|n| {
                if n < buf.capacity() {
                    Err(io::Error::from(io::ErrorKind::UnexpectedEof))
                } else {
                    Ok(n)
                }
            })
    }
}

impl<R: Read> ReadExactExt for R {}

impl PngChunk {
    fn new<T: Read + Seek>(reader: &mut T) -> Result<Self, PngError> {
        let mut buf = [0u8; 4];

        let length = reader.read_u32::<BigEndian>()? as usize;

        reader.read_exact(&mut buf)?;
        let kind = PngChunkKind::try_from(&buf)?;

        if kind == PngChunkKind::Ihdr && length != 13 {
            return Err(PngError::InvalidIhdrLength);
        }

        // Optimization: Only performs one allocation for the data buffer
        let mut data = Vec::new();
        data.reserve_exact(length);
        reader.read_exact_capacity(&mut data)?;

        // Intentionally skip over CRC
        reader.read_exact(&mut buf)?;

        Ok(Self {
            kind,
            data: data.into_boxed_slice(),
        })
    }

    fn write<T: Write>(&self, writer: &mut T) -> io::Result<()> {
        writer.write_u32::<BigEndian>(self.data.len() as u32)?;

        let type_code: &[u8; 4] = self.kind.into();
        writer.write_all(type_code)?;

        writer.write_all(&self.data)?;

        let mut hasher = Hasher::new();
        hasher.update(type_code);
        hasher.update(&self.data);
        writer.write_u32::<BigEndian>(hasher.finalize())?;

        Ok(())
    }

    fn iend() -> Self {
        Self {
            kind: PngChunkKind::Iend,
            data: Box::new([]),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct IhdrData {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    compression: u8,
    filter: u8,
    interlace: u8,
}

impl TryFrom<&PngChunk> for IhdrData {
    type Error = PngError;

    fn try_from(chunk: &PngChunk) -> Result<Self, Self::Error> {
        if chunk.kind != PngChunkKind::Ihdr {
            return Err(PngError::NotIhdr);
        }

        let mut cursor = Cursor::new(chunk.data.as_ref());
        Ok(IhdrData {
            width: cursor.read_u32::<BigEndian>()?,
            height: cursor.read_u32::<BigEndian>()?,
            bit_depth: cursor.read_u8()?,
            color_type: cursor.read_u8()?,
            compression: cursor.read_u8()?,
            filter: cursor.read_u8()?,
            interlace: cursor.read_u8()?,
        })
    }
}

impl From<IhdrData> for PngChunk {
    fn from(ihdr: IhdrData) -> Self {
        // Optimization: Only performs one allocation for the data buffer
        let mut data = Vec::new();
        data.reserve_exact(13);

        // Convert to Options here to ignore Result without using let _ = ... since that's too aggressive
        data.write_u32::<BigEndian>(ihdr.width).ok();
        data.write_u32::<BigEndian>(ihdr.height).ok();
        data.write_u8(ihdr.bit_depth).ok();
        data.write_u8(ihdr.color_type).ok();
        data.write_u8(ihdr.compression).ok();
        data.write_u8(ihdr.filter).ok();
        data.write_u8(ihdr.interlace).ok();

        PngChunk {
            kind: PngChunkKind::Ihdr,
            data: data.into_boxed_slice(),
        }
    }
}

fn catpng<T, U>(pngs: T, level: u8) -> Result<(IhdrData, PngChunk)>
where
    T: IntoIterator<Item = (U, PathBuf)>,
    U: Read + Seek,
{
    let inflate_png = |(mut saved, mut buf): (Option<IhdrData>, Vec<u8>), mut png: U| {
        let mut signature_buf = [0u8; PNG_SIGNATURE.len()];
        if png.read_exact(&mut signature_buf).is_err() || signature_buf != *PNG_SIGNATURE {
            return Err(PngError::InvalidSignature);
        }

        let ihdr = IhdrData::try_from(&PngChunk::new(&mut png)?)?;

        if saved
            .as_ref()
            .is_some_and(|saved| saved.width != ihdr.width)
        {
            return Err(PngError::UnequalWidth);
        }

        buf.append(&mut decompress_to_vec_zlib(&PngChunk::new(&mut png)?.data)?);

        if let Some(ref mut saved) = saved {
            saved.height += ihdr.height;
        } else {
            saved = Some(ihdr);
        }

        Ok((saved, buf))
    };

    pngs.into_iter()
        .try_fold((None, Vec::new()), |a, (reader, path)| {
            inflate_png(a, reader)
                .with_context(|| format!("failed to process `{}`", path.display()))
        })
        .map(|(saved, buf)| {
            (
                saved.expect("output IHDR"),
                PngChunk {
                    kind: PngChunkKind::Idat,
                    data: compress_to_vec_zlib(&buf, level).into_boxed_slice(),
                },
            )
        })
}

fn write_png<T: Write>((ihdr, idat): (IhdrData, PngChunk), writer: &mut T) -> io::Result<()> {
    writer.write_all(PNG_SIGNATURE)?;
    for c in [ihdr.into(), idat, PngChunk::iend()] {
        c.write(writer)?;
    }

    Ok(())
}

fn main() -> Result<()> {
    const USAGE: &str = "Usage: OUTPUT LEVEL INPUT...\n\tOUTPUT: The output file path\n\tLEVEL: The output file compression level (0-10, default: 10)\n\tINPUT: The input file(s)";

    let mut args = env::args().skip(1).peekable();

    let (out_path, out_level, _) = match (
        args.next(),
        args.next().and_then(|s| s.trim().parse().ok()),
        args.peek(),
    ) {
        (Some(path), Some(level), Some(_)) => (path, level, ()),
        _ => {
            eprintln!("{}", USAGE);
            bail!("Invalid argument(s)");
        }
    };

    let mut pngs = Vec::new();
    for path in args {
        pngs.push((BufReader::new(File::open(&path)?), PathBuf::from(path)));
    }

    let (out_ihdr, out_idat) = catpng(pngs, out_level)?;

    dbg!(out_ihdr);

    let mut out_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(out_path)?;

    write_png((out_ihdr, out_idat), &mut out_file)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! ihdr {
        { $w:expr, $h:expr, $b:expr, $c:expr, $z:expr, $f:expr, $i:expr } => {
            IhdrData {
                width: $w,
                height: $h,
                bit_depth: $b,
                color_type: $c,
                compression: $z,
                filter: $f,
                interlace: $i,
            }
        };
    }

    fn png_vec(ihdr: IhdrData, data: &[u8], level: u8) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        write_png(
            (
                ihdr,
                PngChunk {
                    kind: PngChunkKind::Idat,
                    data: compress_to_vec_zlib(data, level).into_boxed_slice(),
                },
            ),
            &mut buf,
        )?;

        Ok(buf)
    }

    fn catpng_and_write(pngs: &[(Vec<u8>, &str)], level: u8) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        write_png(
            catpng(
                pngs.into_iter()
                    .map(|(buf, path)| (Cursor::new(buf), PathBuf::from(path))),
                level,
            )?,
            &mut out,
        )?;
        Ok(out)
    }

    #[test]
    fn concat_1() -> Result<()> {
        let buf = png_vec(ihdr! {69, 50, 0, 0, 0, 0, 0}, &[1, 2, 3], 0)?;
        let expected = buf.clone();

        let out = catpng_and_write(&[(buf, "1")], 0)?;
        assert_eq!(&out, &expected);

        Ok(())
    }

    #[test]
    fn concat_2() -> Result<()> {
        let buf1 = png_vec(ihdr! {69, 25, 0, 0, 0, 0, 0}, &[1, 2, 3], 0)?;
        let buf2 = png_vec(ihdr! {69, 30, 0, 0, 0, 0, 0}, &[4, 5, 6], 0)?;
        let expected = png_vec(ihdr! {69, 55, 0, 0, 0, 0, 0}, &[1, 2, 3, 4, 5, 6], 0)?;

        let out = catpng_and_write(&[(buf1, "1"), (buf2, "2")], 0)?;
        assert_eq!(&out, &expected);

        Ok(())
    }
}
