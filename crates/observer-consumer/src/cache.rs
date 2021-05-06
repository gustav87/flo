use crate::error::Result;
use once_cell::sync::Lazy;
use redis::aio::ConnectionManager;
use redis::{cmd, pipe, AsyncCommands, Client};
use std::fmt::{self, Debug, Formatter};

static CLIENT: Lazy<Client> =
  Lazy::new(|| Client::open(&*crate::env::ENV.redis_url).expect("redis client open"));

#[derive(Clone)]
pub struct Cache {
  conn: ConnectionManager,
}

impl Cache {
  pub async fn connect() -> Result<Self> {
    let conn = CLIENT.get_tokio_connection_manager().await?;
    Ok(Self { conn })
  }

  const SHARD_HASH_PREFIX: &'static str = "flo_observer:shard";
  const SHARD_HASH_FINISHED_SEQ_NUMBER: &'static str = "finished_seq_number";

  pub async fn get_shard_finished_seq(&mut self, shard_id: &str) -> Result<Option<String>> {
    let key = format!("{}:{}", Self::SHARD_HASH_PREFIX, shard_id);
    let res: Option<String> = redis::cmd("HGET")
      .arg(key)
      .arg(Self::SHARD_HASH_FINISHED_SEQ_NUMBER)
      .query_async(&mut self.conn)
      .await?;

    Ok(res)
  }

  pub async fn set_shard_finished_seq(&mut self, shard_id: &str, value: &str) -> Result<()> {
    let key = format!("{}:{}", Self::SHARD_HASH_PREFIX, shard_id);
    redis::cmd("HSET")
      .arg(key)
      .arg(Self::SHARD_HASH_FINISHED_SEQ_NUMBER)
      .arg(value)
      .query_async::<_, ()>(&mut self.conn)
      .await?;
    Ok(())
  }

  const GAME_SET_KEY: &'static str = "flo_observer:games";
  const GAME_HASH_PREFIX: &'static str = "flo_observer:game";
  const GAME_HASH_SHARD_ID: &'static str = "shard_id";
  const GAME_HASH_FINISHED_SEQ_ID: &'static str = "finished_seq_id";

  pub async fn add_game(&mut self, game_id: i32, shard_id: &str) -> Result<()> {
    let game_hash_key = format!("{}:{}", Self::GAME_HASH_PREFIX, game_id);
    pipe()
      .atomic()
      .cmd("SADD")
      .arg(Self::GAME_SET_KEY)
      .arg(&game_id.to_le_bytes() as &[u8])
      .cmd("HSET")
      .arg(&game_hash_key)
      .arg(Self::GAME_HASH_SHARD_ID)
      .arg(shard_id)
      .query_async::<_, ()>(&mut self.conn)
      .await?;
    Ok(())
  }

  pub async fn remove_game(&mut self, game_id: i32) -> Result<()> {
    redis::cmd("SREM")
      .arg(Self::GAME_SET_KEY)
      .arg(&game_id.to_le_bytes() as &[u8])
      .query_async::<_, ()>(&mut self.conn)
      .await?;

    let game_hash_key = format!("{}:{}", Self::GAME_HASH_PREFIX, game_id);
    pipe()
      .atomic()
      .cmd("SREM")
      .arg(Self::GAME_SET_KEY)
      .arg(&game_id.to_le_bytes() as &[u8])
      .cmd("DEL")
      .arg(&game_hash_key)
      .query_async::<_, ()>(&mut self.conn)
      .await?;

    Ok(())
  }

  pub async fn list_games(&mut self) -> Result<Vec<i32>> {
    let list: Vec<Vec<u8>> = redis::cmd("SMEMBERS")
      .arg(Self::GAME_SET_KEY)
      .query_async(&mut self.conn)
      .await?;
    Ok(
      list
        .into_iter()
        .filter_map(|bytes| {
          if bytes.len() == 4 {
            Some(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
          } else {
            None
          }
        })
        .collect(),
    )
  }

  pub async fn set_game_finished_seq_id(&mut self, game_id: i32, value: u32) -> Result<()> {
    let key = format!("{}:{}", Self::GAME_HASH_PREFIX, game_id);
    redis::cmd("HSET")
      .arg(key)
      .arg(Self::GAME_HASH_FINISHED_SEQ_ID)
      .arg(&value.to_le_bytes() as &[u8])
      .query_async::<_, ()>(&mut self.conn)
      .await?;
    Ok(())
  }

  pub async fn get_game_state(&mut self, game_id: i32) -> Result<Option<CacheGameState>> {
    let key = format!("{}:{}", Self::GAME_HASH_PREFIX, game_id);
    let (shard_id, finished_seq_id): (Option<String>, Option<Vec<u8>>) = redis::cmd("HMGET")
      .arg(key)
      .arg(Self::GAME_HASH_SHARD_ID)
      .arg(Self::GAME_HASH_FINISHED_SEQ_ID)
      .query_async(&mut self.conn)
      .await?;
    Ok(
      shard_id
        .and_then(|shard_id| Some((shard_id, finished_seq_id?)))
        .and_then(|(shard_id, bytes)| {
          if bytes.len() == 4 {
            Some(CacheGameState {
              id: game_id,
              shard_id,
              finished_seq_id: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            })
          } else {
            None
          }
        }),
    )
  }
}

impl Debug for Cache {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    f.debug_struct("Cache").finish()
  }
}

#[derive(Debug)]
pub struct CacheGameState {
  pub id: i32,
  pub shard_id: String,
  pub finished_seq_id: u32,
}

#[tokio::test]
async fn test_shared_finished_seq() {
  dotenv::dotenv().unwrap();

  let mut c = Cache::connect().await.unwrap();

  redis::cmd("HDEL")
    .arg(format!("{}:{}", Cache::SHARD_HASH_PREFIX, "FAKE"))
    .arg(Cache::SHARD_HASH_FINISHED_SEQ_NUMBER)
    .query_async::<_, ()>(&mut c.conn)
    .await
    .unwrap();

  let v = c.get_shard_finished_seq("FAKE").await.unwrap();
  assert_eq!(v, None);

  c.set_shard_finished_seq("FAKE", "123").await.unwrap();
  let v = c.get_shard_finished_seq("FAKE").await.unwrap();
  assert_eq!(v, Some("123".to_string()))
}

#[tokio::test]
async fn test_game_set() {
  dotenv::dotenv().unwrap();

  let mut c = Cache::connect().await.unwrap();

  redis::cmd("DEL")
    .arg(Cache::GAME_SET_KEY)
    .query_async::<_, ()>(&mut c.conn)
    .await
    .unwrap();

  let list = c.list_games().await.unwrap();
  assert!(list.is_empty());

  c.add_game(0x11, "a").await.unwrap();
  c.add_game(0x22, "a").await.unwrap();
  c.add_game(0x33, "a").await.unwrap();

  let list = c.list_games().await.unwrap();
  assert_eq!(list, vec![0x11, 0x22, 0x33]);

  c.remove_game(0x22).await.unwrap();

  let list = c.list_games().await.unwrap();
  assert_eq!(list, vec![0x11, 0x33]);
}

#[tokio::test]
async fn test_game_finished_seq() {
  dotenv::dotenv().unwrap();
  let game_id = i32::MAX;

  let mut c = Cache::connect().await.unwrap();

  c.remove_game(game_id).await.unwrap();

  let v = c.get_game_state(game_id).await.unwrap();
  assert!(v.is_none());

  c.add_game(game_id, "shard").await.unwrap();
  c.set_game_finished_seq_id(game_id, 456).await.unwrap();
  let v = c.get_game_state(game_id).await.unwrap().unwrap();
  assert_eq!(v.shard_id, "shard");
  assert_eq!(v.finished_seq_id, 456);
}
