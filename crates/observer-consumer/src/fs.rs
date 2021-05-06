use crate::error::Result;
use bytes::{Buf, BufMut, BytesMut};
use flate2::read::GzDecoder;
use flo_observer::record::{GameRecord, GameRecordData};
use flo_util::binary::{BinDecode, BinEncode};
use flo_util::{BinDecode, BinEncode};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::channel;

const MAX_CHUNK_SIZE: usize = 4 * 1024;
const CHUNK_PREFIX: &'static str = "chunk_";
const CHUNK_TEMP_FILENAME: &'static str = "_chunk";
const ARCHIVE_FILE_NAME: &'static str = "archive.gz";
static DATA_FOLDER: Lazy<PathBuf> = Lazy::new(|| {
  let path = PathBuf::from("./data");
  std::fs::create_dir_all(&path).expect("create data folder");
  path
});

#[derive(Debug)]
pub struct GameDataWriter {
  game_id: i32,
  chunk_id: usize,
  chunk_buf: BytesMut,
  dir: PathBuf,
  chunk_file: Option<File>,
}

impl GameDataWriter {
  pub async fn create(game_id: i32) -> Result<Self> {
    let dir = DATA_FOLDER.join(game_id.to_string());
    fs::create_dir_all(&dir).await?;
    let path = dir.join(CHUNK_TEMP_FILENAME);
    let mut chunk_file = File::create(path).await?;
    Ok(Self {
      game_id,
      chunk_id: 0,
      chunk_buf: BytesMut::with_capacity(MAX_CHUNK_SIZE),
      dir,
      chunk_file: chunk_file.into(),
    })
  }

  pub async fn recover(game_id: i32) -> Result<Self> {
    let r = GameDataReader::open(game_id).await?;
    let path = r.dir.join(CHUNK_TEMP_FILENAME);
    let chunk_file = File::create(path).await?;
    Ok(Self {
      game_id,
      chunk_id: r.next_chunk_id,
      chunk_buf: BytesMut::with_capacity(MAX_CHUNK_SIZE),
      dir: r.dir,
      chunk_file: chunk_file.into(),
    })
  }

  pub async fn write_record(&mut self, data: GameRecordData) -> Result<WriteDestination> {
    assert!(data.encode_len() <= MAX_CHUNK_SIZE);
    let mut r = WriteDestination::CurrentChunk;
    if self.chunk_buf.len() + data.encode_len() > MAX_CHUNK_SIZE {
      self.flush_chunk().await?;
      r = WriteDestination::NewChunk;
    }
    data.encode(&mut self.chunk_buf);
    Ok(r)
  }

  pub async fn write_bytes<T: AsRef<[u8]>>(&mut self, bytes: T) -> Result<WriteDestination> {
    let slice = bytes.as_ref();
    assert!(slice.len() <= MAX_CHUNK_SIZE);
    let mut r = WriteDestination::CurrentChunk;
    if self.chunk_buf.len() + slice.len() > MAX_CHUNK_SIZE {
      self.flush_chunk().await?;
      r = WriteDestination::NewChunk;
    }
    self.chunk_buf.put(slice);
    Ok(r)
  }

  pub async fn sync_all(&mut self) -> Result<()> {
    if self.chunk_buf.is_empty() {
      return Ok(());
    }
    self.flush_chunk().await?;
    Ok(())
  }

  pub async fn build_archive(&mut self) -> Result<()> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io;
    use std::io::prelude::*;
    self.flush_chunk().await?;
    tokio::task::block_in_place(|| {
      let file = std::fs::File::create(self.dir.join(ARCHIVE_FILE_NAME))?;
      let mut encoder = GzEncoder::new(file, Compression::default());
      encoder.write_all(&FileHeader::new(self.game_id).bytes())?;
      for i in 0..self.chunk_id {
        let mut chunk_file = std::fs::File::open(self.dir.join(format!("{}{}", CHUNK_PREFIX, i)))?;
        std::io::copy(&mut chunk_file, &mut encoder)?;
      }
      encoder.flush()?;
      Ok(())
    })
  }

  async fn flush_chunk(&mut self) -> Result<()> {
    if self.chunk_buf.is_empty() {
      return Ok(());
    }

    {
      let mut chunk_file = self.chunk_file.take().unwrap();
      chunk_file.write_all(self.chunk_buf.as_ref()).await?;
      self.chunk_buf.clear();
      chunk_file.sync_all().await?;
    }
    let name = format!("{}{}", CHUNK_PREFIX, self.chunk_id);
    fs::rename(self.dir.join(CHUNK_TEMP_FILENAME), self.dir.join(name)).await?;
    self.chunk_id += 1;
    let chunk_file = File::create(self.dir.join(CHUNK_TEMP_FILENAME)).await?;
    self.chunk_file = Some(chunk_file);
    Ok(())
  }
}

pub enum WriteDestination {
  CurrentChunk,
  NewChunk,
}

pub struct GameDataReader {
  game_id: i32,
  next_chunk_id: usize,
  dir: PathBuf,
}

impl GameDataReader {
  pub async fn open(game_id: i32) -> Result<Self> {
    let dir = DATA_FOLDER.join(game_id.to_string());
    let mut stream = fs::read_dir(&dir).await?;
    let mut max_chunk_id = 0;
    while let Some(entry) = stream.next_entry().await? {
      let file_name = entry.file_name();
      let name = if let Some(v) = file_name.to_str() {
        v
      } else {
        continue;
      };
      if name == CHUNK_TEMP_FILENAME {
        continue;
      }
      if entry.file_type().await?.is_file() && name.starts_with(CHUNK_PREFIX) {
        if name.starts_with(CHUNK_PREFIX) {
          let number = (&name[(CHUNK_PREFIX.len())..]).parse::<usize>();
          if let Ok(number) = number {
            max_chunk_id = std::cmp::max(max_chunk_id, number);
          }
        }
      }
    }
    Ok(Self {
      game_id,
      next_chunk_id: max_chunk_id + 1,
      dir,
    })
  }

  pub fn records(self) -> GameDataReaderRecords {
    GameDataReaderRecords {
      inner: GameDataReaderRecordsInner::Chunks(self),
      current_chunk: None,
      chunk_buf: Cursor::new(vec![]),
    }
  }
}

pub struct GameDataArchiveReader {
  header: FileHeader,
  content: Vec<u8>,
}

impl GameDataArchiveReader {
  pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
    let content = fs::read(path).await?;
    let mut r = GzDecoder::new(Cursor::new(content));

    let mut header_buf: [u8; FileHeader::MIN_SIZE] = [0; FileHeader::MIN_SIZE];
    r.read_exact(&mut header_buf)?;
    let mut s = &header_buf as &[u8];
    let header = FileHeader::decode(&mut s).map_err(crate::error::Error::DecodeArchiveHeader)?;
    let content = tokio::task::block_in_place(|| -> Result<_> {
      let mut content = vec![];
      r.read_to_end(&mut content)?;
      Ok(content)
    })?;

    Ok(Self { header, content })
  }

  pub fn game_id(&self) -> i32 {
    self.header.game_id
  }

  pub fn records(self) -> GameDataReaderRecords {
    GameDataReaderRecords {
      inner: GameDataReaderRecordsInner::Content,
      current_chunk: Some(0),
      chunk_buf: Cursor::new(self.content),
    }
  }
}

pub struct GameDataReaderRecords {
  inner: GameDataReaderRecordsInner,
  current_chunk: Option<usize>,
  chunk_buf: Cursor<Vec<u8>>,
}

enum GameDataReaderRecordsInner {
  Content,
  Chunks(GameDataReader),
}

impl GameDataReaderRecordsInner {
  fn last_chunk_id(&self) -> usize {
    match *self {
      GameDataReaderRecordsInner::Content => 0,
      GameDataReaderRecordsInner::Chunks(ref inner) => inner.next_chunk_id - 1,
    }
  }
}

impl GameDataReaderRecords {
  pub async fn next(&mut self) -> Result<Option<GameRecordData>> {
    if !self.chunk_buf.has_remaining() {
      if !self.read_next_chunk().await? {
        return Ok(None);
      }
    }

    let record = GameRecordData::decode(&mut self.chunk_buf)?;
    Ok(Some(record))
  }

  pub async fn collect_vec(mut self) -> Result<Vec<GameRecordData>> {
    let mut all = vec![];
    while let Some(next) = self.next().await? {
      all.push(next);
    }
    Ok(all)
  }

  async fn read_next_chunk(&mut self) -> Result<bool> {
    if self.current_chunk == Some(self.inner.last_chunk_id()) {
      return Ok(false);
    }

    match self.inner {
      GameDataReaderRecordsInner::Content => unreachable!(),
      GameDataReaderRecordsInner::Chunks(ref mut inner) => {
        let id = self.current_chunk.map(|id| id + 1).unwrap_or(0);

        self.chunk_buf =
          Cursor::new(fs::read(inner.dir.join(format!("{}{}", CHUNK_PREFIX, id))).await?);
        self.current_chunk = id.into();
      }
    }
    Ok(true)
  }
}

#[derive(Debug, BinEncode, BinDecode)]
struct FileHeader {
  #[bin(eq = FileHeader::SIGNATURE)]
  signature: [u8; 4],
  game_id: i32,
}

impl FileHeader {
  const SIGNATURE: &'static [u8] = b"flo\x01";

  fn new(game_id: i32) -> Self {
    let mut buf = [0; 4];
    buf.copy_from_slice(&Self::SIGNATURE);
    Self {
      signature: buf,
      game_id,
    }
  }

  fn bytes(&self) -> [u8; 8] {
    let mut buf = [0; 8];
    let mut s = &mut buf as &mut [u8];
    self.encode(&mut s);
    buf
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_fs() {
  const N: usize = 10000;
  let game_id = i32::MAX;
  fs::remove_dir_all(DATA_FOLDER.join(game_id.to_string()))
    .await
    .ok();

  let records: Vec<_> = (0..N)
    .map(|id| GameRecordData::StopLag(id as i32))
    .collect();

  let mut writer = GameDataWriter::create(game_id).await.unwrap();
  for record in records {
    writer.write_record(record).await.unwrap();
  }
  writer.build_archive().await.unwrap();

  fs::rename(
    writer.dir.join(ARCHIVE_FILE_NAME),
    writer.dir.join("_archive.gz"),
  )
  .await
  .unwrap();

  let mut writer = GameDataWriter::recover(game_id).await.unwrap();
  assert_eq!(writer.chunk_id, 13);
  writer.build_archive().await.unwrap();

  assert_eq!(
    fs::read(writer.dir.join("_archive.gz")).await.unwrap(),
    fs::read(writer.dir.join(ARCHIVE_FILE_NAME)).await.unwrap(),
  );

  // read

  fn validate_records(items: Vec<GameRecordData>) {
    for (i, r) in items.into_iter().enumerate() {
      let v = match r {
        GameRecordData::StopLag(id) => id as usize,
        _ => unreachable!(),
      };
      assert_eq!(i, v);
    }
  }

  let r = GameDataReader::open(game_id).await.unwrap();
  let records = r.records().collect_vec().await.unwrap();
  assert_eq!(records.len(), N);
  validate_records(records);

  let r = GameDataArchiveReader::open(writer.dir.join(ARCHIVE_FILE_NAME))
    .await
    .unwrap();
  let records = r.records().collect_vec().await.unwrap();
  assert_eq!(records.len(), N);
  validate_records(records);
}
