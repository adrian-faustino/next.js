mod extended_key;

use std::{
    collections::{hash_map::Entry, HashMap},
    error::Error,
    fs::create_dir_all,
    path::Path,
    sync::{atomic::AtomicUsize, Arc},
    thread::available_parallelism,
    time::Instant,
};

use anyhow::{anyhow, Context, Result};
use lmdb::{
    Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction, WriteFlags,
};
use tracing::Span;
use turbo_tasks::{backend::CachedTaskType, KeyValuePair, TaskId};

use crate::{
    backend::AnyOperation,
    backing_storage::BackingStorage,
    data::{CachedDataItem, CachedDataItemKey, CachedDataItemValue, CachedDataUpdate},
    utils::chunked_vec::ChunkedVec,
};

const META_KEY_OPERATIONS: u32 = 0;
const META_KEY_NEXT_FREE_TASK_ID: u32 = 1;

struct IntKey([u8; 4]);

impl IntKey {
    fn new(value: u32) -> Self {
        Self(value.to_be_bytes())
    }
}

impl AsRef<[u8]> for IntKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

fn as_u32<E: Error + Send + Sync + 'static>(result: Result<&[u8], E>) -> Result<u32> {
    let bytes = result?;
    let n = bytes.try_into().map(u32::from_be_bytes)?;
    Ok(n)
}

pub struct LmdbBackingStorage {
    env: Environment,
    meta_db: Database,
    data_db: Database,
    forward_task_cache_db: Database,
    reverse_task_cache_db: Database,
    restored_tasks: AtomicUsize,
    restored_cache_entries: AtomicUsize,
}

impl LmdbBackingStorage {
    pub fn new(path: &Path) -> Result<Self> {
        create_dir_all(path)?;
        println!("opening lmdb {:?}", path);
        let env = Environment::new()
            .set_flags(
                EnvironmentFlags::WRITE_MAP
                    | EnvironmentFlags::NO_META_SYNC
                    | EnvironmentFlags::NO_TLS,
            )
            .set_max_readers((available_parallelism().map_or(16, |v| v.get()) * 8) as u32)
            .set_max_dbs(4)
            .set_map_size(20 * 1024 * 1024 * 1024)
            .open(path)?;
        let meta_db = env.create_db(Some("meta"), DatabaseFlags::INTEGER_KEY)?;
        let data_db = env.create_db(Some("data"), DatabaseFlags::INTEGER_KEY)?;
        let forward_task_cache_db =
            env.create_db(Some("forward_task_cache"), DatabaseFlags::empty())?;
        let reverse_task_cache_db =
            env.create_db(Some("reverse_task_cache"), DatabaseFlags::INTEGER_KEY)?;
        Ok(Self {
            env,
            meta_db,
            data_db,
            forward_task_cache_db,
            reverse_task_cache_db,
            restored_tasks: AtomicUsize::new(0),
            restored_cache_entries: AtomicUsize::new(0),
        })
    }

    fn display_db(&self) -> Result<String> {
        use std::fmt::Write;
        let mut result = String::new();
        let tx = self.env.begin_ro_txn()?;
        let mut cursor = tx.open_ro_cursor(self.data_db)?;
        for item in cursor.iter() {
            let (key, value) = item?;
            let task_id = u32::from_be_bytes(key.try_into()?);
            let data: Vec<CachedDataItem> = pot::from_slice(value)?;
            write!(result, "### Task {task_id}\n{data:#?}\n\n")?;
        }
        Ok(result)
    }
}

impl BackingStorage for LmdbBackingStorage {
    fn startup(&self) {
        // println!(
        //     "Database content:\n{}",
        //     self.display_db().unwrap_or_default()
        // );
    }

    fn next_free_task_id(&self) -> TaskId {
        fn get(this: &LmdbBackingStorage) -> Result<u32> {
            let tx = this.env.begin_rw_txn()?;
            let next_free_task_id =
                as_u32(tx.get(this.meta_db, &IntKey::new(META_KEY_NEXT_FREE_TASK_ID)))?;
            Ok(next_free_task_id)
        }
        TaskId::from(get(self).unwrap_or(1))
    }

    fn uncompleted_operations(&self) -> Vec<AnyOperation> {
        fn get(this: &LmdbBackingStorage) -> Result<Vec<AnyOperation>> {
            let tx = this.env.begin_ro_txn()?;
            let operations = tx.get(this.meta_db, &IntKey::new(META_KEY_OPERATIONS))?;
            let operations = pot::from_slice(operations)?;
            Ok(operations)
        }
        get(self).unwrap_or_default()
    }

    fn save_snapshot(
        &self,
        operations: Vec<Arc<AnyOperation>>,
        task_cache_updates: ChunkedVec<(Arc<CachedTaskType>, TaskId)>,
        data_updates: ChunkedVec<CachedDataUpdate>,
    ) -> Result<()> {
        let restored_cache_entries = self
            .restored_cache_entries
            .fetch_and(0, std::sync::atomic::Ordering::Relaxed);
        println!(
            "Restored {} tasks, {} cache entries",
            self.restored_tasks
                .fetch_and(0, std::sync::atomic::Ordering::Relaxed),
            restored_cache_entries
        );
        if restored_cache_entries > 0 {
            for (i, (task_type, _)) in task_cache_updates.iter().enumerate() {
                println!("New Task: {task_type:?}");
                if i > 10 {
                    break;
                }
            }
        }
        println!(
            "Persisting {} operations, {} task cache updates, {} data updates...",
            operations.len(),
            task_cache_updates.len(),
            data_updates.len()
        );
        let start = Instant::now();
        let mut op_count = 0;
        let mut tx = self.env.begin_rw_txn()?;
        let mut next_task_id =
            as_u32(tx.get(self.meta_db, &IntKey::new(META_KEY_NEXT_FREE_TASK_ID))).unwrap_or(1);
        for (task_type, task_id) in task_cache_updates.iter() {
            let task_id = **task_id;
            let task_type_bytes = pot::to_vec(&**task_type)
                .with_context(|| anyhow!("Unable to serialize task cache key {task_type:?}"))?;
            #[cfg(feature = "verify_serialization")]
            {
                let deserialize: Result<CachedTaskType, _> = serde_path_to_error::deserialize(
                    &mut pot::de::SymbolList::new().deserializer_for_slice(&task_type_bytes)?,
                );
                if let Err(err) = deserialize {
                    println!(
                        "Task type would not be deserializable {task_id}: {err:?}\n{task_type:#?}"
                    );
                    panic!("Task type would not be deserializable {task_id}: {err:?}");
                }
            }
            extended_key::put(
                &mut tx,
                self.forward_task_cache_db,
                &task_type_bytes,
                &task_id.to_be_bytes(),
                WriteFlags::empty(),
            )
            .with_context(|| anyhow!("Unable to write task cache {task_type:?} => {task_id}"))?;
            tx.put(
                self.reverse_task_cache_db,
                &IntKey::new(task_id),
                &task_type_bytes,
                WriteFlags::empty(),
            )
            .with_context(|| anyhow!("Unable to write task cache {task_id} => {task_type:?}"))?;
            op_count += 2;
            next_task_id = next_task_id.max(task_id + 1);
        }
        tx.put(
            self.meta_db,
            &IntKey::new(META_KEY_NEXT_FREE_TASK_ID),
            &next_task_id.to_be_bytes(),
            WriteFlags::empty(),
        )
        .with_context(|| anyhow!("Unable to write next free task id"))?;
        let operations =
            pot::to_vec(&operations).with_context(|| anyhow!("Unable to serialize operations"))?;
        tx.put(
            self.meta_db,
            &IntKey::new(META_KEY_OPERATIONS),
            &operations,
            WriteFlags::empty(),
        )
        .with_context(|| anyhow!("Unable to write operations"))?;
        op_count += 2;

        let mut updated_items: HashMap<TaskId, HashMap<CachedDataItemKey, CachedDataItemValue>> =
            HashMap::new();
        for CachedDataUpdate { task, key, value } in data_updates.into_iter() {
            let data = match updated_items.entry(task) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let mut map = HashMap::new();
                    if let Ok(old_data) = tx.get(self.data_db, &IntKey::new(*task)) {
                        let old_data: Vec<CachedDataItem> = match pot::from_slice(old_data) {
                            Ok(d) => d,
                            Err(_) => serde_path_to_error::deserialize(
                                &mut pot::de::SymbolList::new().deserializer_for_slice(old_data)?,
                            )
                            .with_context(|| {
                                anyhow!("Unable to deserialize old value of {task}: {old_data:?}")
                            })?,
                        };
                        for item in old_data {
                            let (key, value) = item.into_key_and_value();
                            map.insert(key, value);
                        }
                    }
                    entry.insert(map)
                }
            };
            if let Some(value) = value {
                data.insert(key, value);
            } else {
                data.remove(&key);
            }
        }
        for (task_id, data) in updated_items {
            let mut vec: Vec<CachedDataItem> = data
                .into_iter()
                .map(|(key, value)| CachedDataItem::from_key_and_value(key, value))
                .collect();
            let value = match pot::to_vec(&vec) {
                #[cfg(not(feature = "verify_serialization"))]
                Ok(value) => value,
                _ => {
                    let mut error = Ok(());
                    vec.retain(|item| {
                        let mut buf = Vec::<u8>::new();
                        let mut symbol_map = pot::ser::SymbolMap::new();
                        let mut serializer = symbol_map.serializer_for(&mut buf).unwrap();
                        if let Err(err) = serde_path_to_error::serialize(item, &mut serializer) {
                            if item.is_optional() {
                                println!("Skipping non-serializable optional item: {item:?}");
                            } else {
                                error = Err(err).context({
                                    anyhow!(
                                        "Unable to serialize data item for {task_id}: {item:#?}"
                                    )
                                });
                            }
                            false
                        } else {
                            #[cfg(feature = "verify_serialization")]
                            {
                                let deserialize: Result<CachedDataItem, _> =
                                    serde_path_to_error::deserialize(
                                        &mut pot::de::SymbolList::new()
                                            .deserializer_for_slice(&buf)
                                            .unwrap(),
                                    );
                                if let Err(err) = deserialize {
                                    println!(
                                        "Data item would not be deserializable {task_id}: \
                                         {err:?}\n{item:#?}"
                                    );
                                    return false;
                                }
                            }
                            true
                        }
                    });
                    error?;

                    pot::to_vec(&vec).with_context(|| {
                        anyhow!("Unable to serialize data items for {task_id}: {vec:#?}")
                    })?
                }
            };
            tx.put(
                self.data_db,
                &IntKey::new(*task_id),
                &value,
                WriteFlags::empty(),
            )
            .with_context(|| anyhow!("Unable to write data items for {task_id}"))?;
            op_count += 1;
        }
        tx.commit()
            .with_context(|| anyhow!("Unable to commit operations"))?;
        println!(
            "Persisted {op_count} db entries after {:?}",
            start.elapsed()
        );
        Ok(())
    }

    fn forward_lookup_task_cache(&self, task_type: &CachedTaskType) -> Option<TaskId> {
        let span = tracing::trace_span!("forward lookup task cache", key_bytes = 0usize).entered();
        fn lookup(
            this: &LmdbBackingStorage,
            task_type: &CachedTaskType,
            span: &Span,
        ) -> Result<Option<TaskId>> {
            let tx = this.env.begin_ro_txn()?;
            let task_type = pot::to_vec(task_type)?;
            span.record("key_bytes", task_type.len());
            let bytes = match extended_key::get(&tx, this.forward_task_cache_db, &task_type) {
                Ok(result) => result,
                Err(err) => {
                    if err == lmdb::Error::NotFound {
                        return Ok(None);
                    } else {
                        return Err(err.into());
                    }
                }
            };
            let bytes = bytes.try_into()?;
            let id = TaskId::from(u32::from_be_bytes(bytes));
            tx.commit()?;
            Ok(Some(id))
        }
        let id = lookup(self, task_type, &span)
            .inspect_err(|err| println!("Looking up task id for {task_type:?} failed: {err:?}"))
            .ok()??;
        self.restored_cache_entries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(id)
    }

    fn reverse_lookup_task_cache(&self, task_id: TaskId) -> Option<Arc<CachedTaskType>> {
        let span = tracing::trace_span!("reverse lookup task cache", bytes = 0usize).entered();
        fn lookup(
            this: &LmdbBackingStorage,
            task_id: TaskId,
            span: &Span,
        ) -> Result<Option<Arc<CachedTaskType>>> {
            let tx = this.env.begin_ro_txn()?;
            let bytes = match tx.get(this.reverse_task_cache_db, &IntKey::new(*task_id)) {
                Ok(bytes) => bytes,
                Err(err) => {
                    if err == lmdb::Error::NotFound {
                        return Ok(None);
                    } else {
                        return Err(err.into());
                    }
                }
            };
            span.record("bytes", bytes.len());
            let result = pot::from_slice(bytes)?;
            tx.commit()?;
            Ok(Some(result))
        }
        let result = lookup(self, task_id, &span)
            .inspect_err(|err| println!("Looking up task type for {task_id} failed: {err:?}"))
            .ok()??;
        self.restored_cache_entries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(result)
    }

    fn lookup_data(&self, task_id: TaskId) -> Vec<CachedDataItem> {
        let span = tracing::trace_span!("restore data", bytes = 0usize, items = 0usize).entered();
        fn lookup(
            this: &LmdbBackingStorage,
            task_id: TaskId,
            span: &Span,
        ) -> Result<Vec<CachedDataItem>> {
            let tx = this.env.begin_ro_txn()?;
            let bytes = match tx.get(this.data_db, &IntKey::new(*task_id)) {
                Ok(bytes) => bytes,
                Err(err) => {
                    if err == lmdb::Error::NotFound {
                        return Ok(Vec::new());
                    } else {
                        return Err(err.into());
                    }
                }
            };
            span.record("bytes", bytes.len());
            let result: Vec<CachedDataItem> = pot::from_slice(bytes)?;
            span.record("items", result.len());
            tx.commit()?;
            Ok(result)
        }
        let result = lookup(self, task_id, &span)
            .inspect_err(|err| println!("Looking up data for {task_id} failed: {err:?}"))
            .unwrap_or_default();
        if !result.is_empty() {
            self.restored_tasks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }
}