use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io::{BufReader, Read, Seek, SeekFrom};
use adler32::RollingAdler32;
use byteorder::{BE, ByteOrder, LE, ReadBytesExt};
use compress::zlib;
use encoding_rs::{Encoding, UTF_16LE, UTF_8};
use regex::Regex;
use ripemd::{Digest, Ripemd128, Ripemd128Core};
use salsa20::Salsa20;
use salsa20::cipher::{KeyIvInit, StreamCipher};
use salsa20::cipher::crypto_common::Output;

use crate::{Error, mdx::Mdx, Result};
use crate::mdx::{BlockEntryInfo, KeyEntry, KeyMaker, Reader, RecordOffset};

#[derive(Debug)]
struct KeyBlockHeader {
	// block_num: usize,
	// entry_num: usize,
	// decompressed_size: usize,
	block_info_size: usize,
	key_block_size: usize,
}

#[derive(Debug)]
enum Version {
	V1,
	V2,
}

impl Version {
	#[inline]
	fn read_number(&self, reader: &mut Reader) -> Result<usize>
	{
		let number = match self {
			Version::V1 => reader.read_u32::<BE>()? as usize,
			Version::V2 => reader.read_u64::<BE>()? as usize,
		};
		Ok(number)
	}
	#[inline]
	#[allow(unused)]
	fn byte_number(&self, data: &[u8]) -> (usize, usize)
	{
		match self {
			Version::V1 => (BE::read_u32(data) as usize, 4),
			Version::V2 => (BE::read_u64(data) as usize, 8),
		}
	}
}

fn read_keys(s: &str) -> HashMap<String, String>
{
	let re = Regex::new(r#"(\w+)="((.|\r\n|[\r\n])*?)""#).unwrap();
	let mut attrs = HashMap::new();
	for cap in re.captures_iter(s) {
		attrs.insert(cap[1].to_string(), cap[2].to_string());
	}
	attrs
}

#[derive(Debug)]
struct Header {
	version: Version,
	encrypted: u8,
	encoding: &'static Encoding,
	title: String,
}

#[inline]
fn read_buf(reader: &mut impl Read, len: usize) -> Result<Vec<u8>>
{
	let mut buf = vec![0; len];
	reader.read_exact(&mut buf)?;
	Ok(buf)
}

#[inline]
fn check_adler32(data: &[u8], checksum: u32) -> Result<()>
{
	if RollingAdler32::from_buffer(data).hash() != checksum {
		return Err(Error::InvalidCheckSum("header"));
	}
	Ok(())
}

fn read_header(reader: &mut Reader, default_encoding: &'static Encoding) -> Result<Header>
{
	let bytes = reader.read_u32::<BE>()?;
	let info_buf = read_buf(reader, bytes as usize)?;
	let checksum = reader.read_u32::<LE>()?;
	check_adler32(&info_buf, checksum)?;

	let info = UTF_16LE.decode(&info_buf).0;
	let attrs = read_keys(&info);

	let version_str = attrs
		.get("GeneratedByEngineVersion")
		.ok_or(Error::NoVersion)?
		.trim();
	let version = version_str[0..1]
		.parse::<u8>()
		.or(Err(Error::InvalidVersion(version_str.to_owned())))?;


	let title = attrs
		.get("Title")
		.ok_or(Error::NoTitle)?
		.trim()
		.to_owned();

	let version = match version {
		1 => Version::V1,
		2 => Version::V2,
		3 |
		_ => return Err(Error::UnsupportedVersion(version)),
	};

	let encrypted = attrs
		.get("Encrypted")
		.and_then(|x| match x == "Yes" {
			true => Some(1_u8),
			false => x.as_str().parse().ok(),
		})
		.unwrap_or(0);

	let encoding = if let Some(encoding) = attrs.get("Encoding") {
		if encoding.is_empty() {
			default_encoding
		} else {
			Encoding::for_label(encoding.as_bytes())
				.ok_or(Error::InvalidEncoding(encoding.clone()))?
		}
	} else {
		default_encoding
	};
	Ok(Header {
		version,
		encrypted,
		encoding,
		title,
	})
}

fn read_key_block_header_v1(reader: &mut Reader) -> Result<KeyBlockHeader>
{
	let buf = read_buf(reader, 16)?;
	// let block_num = BE::read_u32(&buf[0..4]);
	// let entry_num = BE::read_u32(&buf[4..8]);
	let block_info_size = BE::read_u32(&buf[8..12]);
	let key_block_size = BE::read_u32(&buf[12..16]);

	Ok(KeyBlockHeader {
		// block_num: block_num as usize,
		// entry_num: entry_num as usize,
		// decompressed_size: block_info_size as usize,
		block_info_size: block_info_size as usize,
		key_block_size: key_block_size as usize,
	})
}

fn read_key_block_header_v2(reader: &mut Reader) -> Result<KeyBlockHeader>
{
	let buf = read_buf(reader, 40)?;
	let checksum = reader.read_u32::<BE>()?;
	check_adler32(&buf, checksum)?;

	// let block_num = BE::read_u64(&buf[0..8]);
	// let entry_num = BE::read_u64(&buf[8..16]);
	// let decompressed_size = BE::read_u64(&buf[16..24]);
	let block_info_size = BE::read_u64(&buf[24..32]);
	let key_block_size = BE::read_u64(&buf[32..40]);

	Ok(KeyBlockHeader {
		// block_num: block_num as usize,
		// entry_num: entry_num as usize,
		// decompressed_size: decompressed_size as usize,
		block_info_size: block_info_size as usize,
		key_block_size: key_block_size as usize,
	})
}

fn fast_decrypt(encrypted: &[u8], key: &[u8]) -> Vec<u8>
{
	let mut buf = Vec::from(encrypted);
	let mut prev = 0x36;
	for i in 0..buf.len() {
		let mut t = buf[i] >> 4 | buf[i] << 4;
		t = t ^ prev ^ (i as u8) ^ key[i % key.len()];
		prev = buf[i];
		buf[i] = t;
	}
	buf
}

fn read_key_block_infos(reader: &mut Reader, size: usize, header: &Header) -> Result<Vec<BlockEntryInfo>>
{
	let buf = read_buf(reader, size)?;
	//decrypt
	let key_block_info = match header.version {
		Version::V1 => buf,
		Version::V2 => {
			if buf[0..4] != [2, 0, 0, 0] {
				return Err(Error::InvalidData);
			}
			let checksum = BE::read_u32(&buf[4..8]);
			let mut info = vec![];
			if header.encrypted == 2 {
				let mut v = Vec::from(&buf[4..8]);
				let value: u32 = 0x3695;
				v.extend_from_slice(&value.to_le_bytes());
				let mut md = Ripemd128::default();
				md.update(v);
				let key = md.finalize();
				let decrypted = fast_decrypt(&buf[8..], key.as_slice());
				zlib::Decoder::new(BufReader::new(decrypted.as_slice()))
					.read_to_end(&mut info)?;
			} else {
				zlib::Decoder::new(&buf[8..])
					.read_to_end(&mut info)?;
			}
			check_adler32(&info, checksum)?;
			info
		}
	};
	let key_blocks = decode_key_blocks(&key_block_info, header)?;
	Ok(key_blocks)
}

fn decode_key_blocks(data: &[u8], header: &Header)
	-> Result<Vec<BlockEntryInfo>>
{
	#[inline]
	fn read_size(data: &[u8], header: &Header) -> (usize, usize)
	{
		match header.version {
			Version::V1 => (BE::read_u32(&data[0..4]) as usize, 4),
			Version::V2 => (BE::read_u64(&data[0..8]) as usize, 8),
		}
	}
	#[inline]
	fn read_num_bytes(data: &[u8], header: &Header) -> (usize, usize)
	{
		match header.version {
			Version::V1 => (data[0] as usize, 1),
			Version::V2 => (BE::read_u16(&data[0..2]) as usize, 2)
		}
	}
	#[inline]
	fn text_bytes(header: &Header, bytes: usize) -> usize
	{
		let text_size = match header.version {
			Version::V1 => bytes,
			Version::V2 => bytes + 1,
		};
		if header.encoding == UTF_16LE {
			text_size * 2
		} else {
			text_size
		}
	}
	#[inline]
	#[allow(unused)]
	fn extract_text(data: &[u8], header: &Header, bytes: usize) -> (String, usize)
	{
		let text_size = match header.version {
			Version::V1 => bytes,
			Version::V2 => bytes + 1,
		};
		let bytes = if header.encoding == UTF_16LE {
			text_size * 2
		} else {
			text_size
		};
		let text = header.encoding
			.decode(&data[..text_size])
			.0
			.trim_matches(char::from(0))
			.to_string();
		(text, bytes)
	}

	let mut key_block_info_list = vec![];
	let mut slice = data;
	while !slice.is_empty() {
		let (_num_entries, delta) = read_size(slice, header);
		slice = &slice[delta..];
		let (bytes, delta) = read_num_bytes(slice, header);
		slice = &slice[delta..];
		let delta = text_bytes(header, bytes);
		slice = &slice[delta..];
		let (bytes, delta) = read_num_bytes(slice, header);
		slice = &slice[delta..];
		let delta = text_bytes(header, bytes);
		slice = &slice[delta..];
		let (compressed_size, delta) = read_size(slice, header);
		slice = &slice[delta..];
		let (decompressed_size, delta) = read_size(slice, header);
		slice = &slice[delta..];
		key_block_info_list.push(BlockEntryInfo {
			compressed_size,
			decompressed_size,
		});
	}
	Ok(key_block_info_list)
}

fn decode_block(slice: &[u8], compressed_size: usize, decompressed_size: usize) -> Result<Vec<u8>>
{
	#[inline]
	fn make_key(data: &[u8]) -> Output<Ripemd128Core>
	{
		let mut md = Ripemd128::default();
		md.update(&data[4..8]);
		md.finalize()
	}

	let enc = LE::read_u32(&slice[0..4]);
	let checksum_bytes = &slice[4..8];
	let checksum = BE::read_u32(checksum_bytes);
	let encryption_method = (enc >> 4) & 0xf;
	// let encryption_size = (enc >> 8) & 0xff;
	let compress_method = enc & 0xf;

	let encrypted = &slice[8..compressed_size];
	let compressed: Vec<u8> = match encryption_method {
		0 => Vec::from(encrypted),
		1 => fast_decrypt(encrypted, make_key(checksum_bytes).as_slice()),
		2 => {
			let mut decrypt = Vec::from(encrypted);
			let mut cipher = Salsa20::new(make_key(checksum_bytes).as_slice().into(), &[0; 8].into());
			cipher.apply_keystream(&mut decrypt);
			decrypt
		}
		_ => return Err(Error::InvalidEncryptMethod(encryption_method)),
	};

	let decompressed = match compress_method {
		0 => compressed,
		1 => {
			let mut decompressed = vec![0; decompressed_size];
			let (result, err) = rust_lzo::LZOContext::decompress_to_slice(&compressed, &mut decompressed);
			if err != rust_lzo::LZOError::OK {
				return Err(Error::InvalidData);
			}
			Vec::from(result)
		},
		2 => {
			let mut v = vec![];
			zlib::Decoder::new(&compressed[..]).read_to_end(&mut v)
				.or(Err(Error::InvalidData))?;
			v
		}
		_ => return Err(Error::InvalidCompressMethod(compress_method)),
	};

	check_adler32(&decompressed, checksum)?;
	Ok(decompressed)
}

fn read_key_entries(reader: &mut Reader, size: usize, header: &Header,
	entry_infos: Vec<BlockEntryInfo>, key_maker: &dyn KeyMaker, resource: bool)
	-> Result<Vec<KeyEntry>>
{
	let data = read_buf(reader, size)?;

	let mut entries = vec![];
	let mut slice = data.as_slice();
	for info in entry_infos {
		let decompressed = decode_block(
			slice, info.compressed_size, info.decompressed_size)?;
		slice = &slice[info.compressed_size..];

		let mut entries_slice = decompressed.as_slice();
		while !entries_slice.is_empty() {
			let (offset, delta) = match header.version {
				Version::V1 => (BE::read_u32(entries_slice) as usize, 4),
				Version::V2 => (BE::read_u64(entries_slice) as usize, 8),
			};
			entries_slice = &entries_slice[delta..];
			let (text, idx) = decode_slice_string(entries_slice, header.encoding)?;
			let text = key_maker.make(&text, resource);
			entries.push(KeyEntry { offset, text });
			entries_slice = &entries_slice[idx..];
		}
	}
	entries.sort_by(|a, b| a.text.cmp(&b.text));

	Ok(entries)
}

fn read_record_blocks(reader: &mut Reader, header: &Header)
	-> Result<Vec<BlockEntryInfo>>
{
	let version = &header.version;
	let num_records = version.read_number(reader)?;
	let _num_entries = version.read_number(reader)?;
	let _record_info_size = version.read_number(reader)?;
	let _record_data_size = version.read_number(reader)?;
	let mut records = vec![];
	for _i in 0..num_records {
		let compressed_size = version.read_number(reader)?;
		let decompressed_size = version.read_number(reader)?;
		records.push(BlockEntryInfo { compressed_size, decompressed_size })
	}
	Ok(records)
}

pub(crate) fn load(mut reader: Reader, default_encoding: &'static Encoding,
	cache: bool, key_maker: &dyn KeyMaker, resource: bool) -> Result<Mdx>
{
	let header = read_header(&mut reader, default_encoding)?;
	let key_block_header = match &header.version {
		Version::V1 => read_key_block_header_v1(&mut reader)?,
		Version::V2 => read_key_block_header_v2(&mut reader)?,
	};
	let key_block_infos = read_key_block_infos(
		&mut reader,
		key_block_header.block_info_size,
		&header)?;

	let key_entries = read_key_entries(
		&mut reader,
		key_block_header.key_block_size,
		&header,
		key_block_infos,
		key_maker,
		resource)?;

	let records_info = read_record_blocks(
		&mut reader,
		&header)?;

	let record_block_offset = reader.stream_position()?;

	Ok(Mdx {
		encoding: header.encoding,
		title: header.title,
		encrypted: header.encrypted,
		key_entries,
		records_info,
		reader,
		record_block_offset,
		record_cache: if cache { Some(HashMap::new()) } else { None },
	})
}

fn record_offset(records_info: &Vec<BlockEntryInfo>, entry: &KeyEntry) -> Option<RecordOffset> {
	let mut block_offset = 0;
	let mut buf_offset = 0;
	for info in records_info {
		if entry.offset < block_offset + info.decompressed_size {
			return Some(RecordOffset {
				buf_offset,
				block_offset: entry.offset - block_offset,
				record_size: info.compressed_size,
				decomp_size: info.decompressed_size,
			});
		} else {
			block_offset += info.decompressed_size;
			buf_offset += info.compressed_size;
		}
	}
	None
}

fn find_definition(mdx: &mut Mdx, offset: RecordOffset) -> Result<Cow<[u8]>>
{
	#[inline]
	fn read_record(reader: &mut Reader, record_block_offset: u64,
		offset: RecordOffset) -> Result<Vec<u8>>
	{
		reader.seek(SeekFrom::Start(record_block_offset + offset.buf_offset as u64))?;
		let data = read_buf(reader, offset.record_size)?;
		decode_block(&data, offset.record_size, offset.decomp_size)
	}
	let block_offset = offset.block_offset;
	if let Some(cache) = &mut mdx.record_cache {
		let data = match cache.entry(offset.buf_offset) {
			Entry::Occupied(o) => o.into_mut(),
			Entry::Vacant(v) => {
				let reader = &mut mdx.reader;
				let decompressed = read_record(reader, mdx.record_block_offset, offset)?;
				v.insert(decompressed)
			}
		};
		Ok(Cow::Borrowed(&data[block_offset..]))
	} else {
		let reader = &mut mdx.reader;
		let mut data = read_record(reader, mdx.record_block_offset, offset)?;
		if block_offset != 0 {
			data = Vec::from(&data[block_offset..]);
		}
		Ok(Cow::Owned(data))
	}
}

pub(crate) fn lookup_record<'a>(mdx: &'a mut Mdx, key: &str) -> Result<Option<Cow<'a, [u8]>>>
{
	if let Ok(idx) = mdx.key_entries.binary_search_by(|entry| entry.text.as_str().cmp(key)) {
		let entry = &mdx.key_entries[idx];
		if let Some(offset) = record_offset(&mdx.records_info, entry) {
			let slice = find_definition(mdx, offset)?;
			return Ok(Some(slice));
		}
	}
	Ok(None)
}

pub(crate) fn decode_slice_string<'a>(slice: &'a [u8],
	encoding: &'static Encoding) -> Result<(Cow<'a, str>, usize)>
{
	let (idx, delta) = if encoding == UTF_16LE {
		let mut found = None;
		for i in (0..slice.len()).step_by(2) {
			if slice[i] == 0 && slice[i + 1] == 0 {
				found = Some(i);
				break;
			}
		}
		if let Some(idx) = found {
			(idx, 2)
		} else {
			return Err(Error::InvalidData);
		}
	} else if encoding == UTF_8 {
		let idx = slice
			.iter()
			.position(|b| *b == 0)
			.ok_or(Error::InvalidData)?;
		(idx, 1)
	} else {
		return Err(Error::InvalidEncoding(encoding.name().to_owned()));
	};

	let text = encoding.decode(&slice[..idx]).0;
	Ok((text, idx + delta))
}
