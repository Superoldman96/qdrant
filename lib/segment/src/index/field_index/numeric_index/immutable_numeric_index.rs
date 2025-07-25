use std::collections::BTreeSet;
use std::ops::Bound;
use std::path::PathBuf;
#[cfg(feature = "rocksdb")]
use std::sync::Arc;

use bitvec::vec::BitVec;
use common::ext::BitSliceExt as _;
use common::types::PointOffsetType;
use gridstore::Blob;
#[cfg(feature = "rocksdb")]
use parking_lot::RwLock;
#[cfg(feature = "rocksdb")]
use rocksdb::DB;

use super::mmap_numeric_index::MmapNumericIndex;
use super::mutable_numeric_index::InMemoryNumericIndex;
use super::{Encodable, HISTOGRAM_MAX_BUCKET_SIZE, HISTOGRAM_PRECISION};
use crate::common::Flusher;
use crate::common::operation_error::{OperationError, OperationResult};
#[cfg(feature = "rocksdb")]
use crate::common::rocksdb_buffered_delete_wrapper::DatabaseColumnScheduledDeleteWrapper;
#[cfg(feature = "rocksdb")]
use crate::common::rocksdb_wrapper::DatabaseColumnWrapper;
use crate::index::field_index::histogram::{Histogram, Numericable, Point};
use crate::index::field_index::immutable_point_to_values::ImmutablePointToValues;
use crate::index::field_index::mmap_point_to_values::MmapValue;
use crate::index::payload_config::StorageType;

pub struct ImmutableNumericIndex<T: Encodable + Numericable + MmapValue + Default> {
    map: NumericKeySortedVec<T>,
    histogram: Histogram<T>,
    points_count: usize,
    max_values_per_point: usize,
    point_to_values: ImmutablePointToValues<T>,
    // Backing storage, source of state, persists deletions
    storage: Storage<T>,
}

enum Storage<T: Encodable + Numericable + MmapValue + Default> {
    #[cfg(feature = "rocksdb")]
    RocksDb(DatabaseColumnScheduledDeleteWrapper),
    Mmap(Box<MmapNumericIndex<T>>),
}

pub(super) struct NumericKeySortedVec<T: Encodable + Numericable> {
    data: Vec<Point<T>>,
    deleted: BitVec,
    deleted_count: usize,
}

pub(super) struct NumericKeySortedVecIterator<'a, T: Encodable + Numericable> {
    set: &'a NumericKeySortedVec<T>,
    start_index: usize,
    end_index: usize,
}

impl<T: Encodable + Numericable> NumericKeySortedVec<T> {
    fn from_btree_set(map: BTreeSet<Point<T>>) -> Self {
        Self {
            deleted: BitVec::repeat(false, map.len()),
            data: map.into_iter().collect(),
            deleted_count: 0,
        }
    }

    fn len(&self) -> usize {
        self.data.len() - self.deleted_count
    }

    fn remove(&mut self, key: &Point<T>) -> bool {
        if let Ok(index) = self.data.binary_search(key) {
            if let Some(is_deleted) = self.deleted.get_mut(index).as_deref_mut() {
                if !*is_deleted {
                    self.deleted_count += 1;
                    *is_deleted = true;
                }
                return true;
            }
        }
        false
    }

    fn values_range(
        &self,
        start_bound: Bound<Point<T>>,
        end_bound: Bound<Point<T>>,
    ) -> NumericKeySortedVecIterator<'_, T> {
        let start_index = self.find_start_index(start_bound);
        let end_index = self.find_end_index(start_index, end_bound);
        NumericKeySortedVecIterator {
            set: self,
            start_index,
            end_index,
        }
    }

    pub(super) fn find_start_index(&self, bound: Bound<Point<T>>) -> usize {
        match bound {
            Bound::Included(bound) => self.data.binary_search(&bound).unwrap_or_else(|idx| idx),
            Bound::Excluded(bound) => match self.data.binary_search(&bound) {
                Ok(idx) => idx + 1,
                Err(idx) => idx,
            },
            Bound::Unbounded => 0,
        }
    }

    pub(super) fn find_end_index(&self, start: usize, bound: Bound<Point<T>>) -> usize {
        if start >= self.data.len() {
            // the range `end` should never be less than `start`
            return start;
        }
        match bound {
            Bound::Included(bound) => match self.data[start..].binary_search(&bound) {
                Ok(idx) => idx + 1 + start,
                Err(idx) => idx + start,
            },
            Bound::Excluded(bound) => {
                let end_bound = self.data[start..].binary_search(&bound);
                end_bound.unwrap_or_else(|idx| idx) + start
            }
            Bound::Unbounded => self.data.len(),
        }
    }
}

impl<T: Encodable + Numericable> Iterator for NumericKeySortedVecIterator<'_, T> {
    type Item = Point<T>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.start_index < self.end_index {
            let key = self.set.data[self.start_index].clone();
            let deleted = self.set.deleted.get_bit(self.start_index).unwrap_or(true);
            self.start_index += 1;
            if deleted {
                continue;
            }
            return Some(key);
        }
        None
    }
}

impl<T: Encodable + Numericable> DoubleEndedIterator for NumericKeySortedVecIterator<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        while self.start_index < self.end_index {
            let key = self.set.data[self.end_index - 1].clone();
            let deleted = self.set.deleted.get_bit(self.end_index - 1).unwrap_or(true);
            self.end_index -= 1;
            if deleted {
                continue;
            }
            return Some(key);
        }
        None
    }
}

impl<T: Encodable + Numericable + MmapValue + Send + Sync + Default> ImmutableNumericIndex<T>
where
    Vec<T>: Blob,
{
    /// Open immutable numeric index from RocksDB storage
    ///
    /// Note: after opening, the data must be loaded into memory separately using [`load`].
    #[cfg(feature = "rocksdb")]
    pub(super) fn open_rocksdb(db: Arc<RwLock<DB>>, field: &str) -> Self {
        let store_cf_name = super::numeric_index_storage_cf_name(field);
        let db_wrapper = DatabaseColumnScheduledDeleteWrapper::new(DatabaseColumnWrapper::new(
            db,
            &store_cf_name,
        ));
        Self {
            map: NumericKeySortedVec {
                data: Default::default(),
                deleted: BitVec::new(),
                deleted_count: 0,
            },
            histogram: Histogram::new(HISTOGRAM_MAX_BUCKET_SIZE, HISTOGRAM_PRECISION),
            points_count: 0,
            max_values_per_point: 1,
            point_to_values: Default::default(),
            storage: Storage::RocksDb(db_wrapper),
        }
    }

    /// Open immutable numeric index from mmap storage
    ///
    /// Note: after opening, the data must be loaded into memory separately using [`load`].
    pub(super) fn open_mmap(index: MmapNumericIndex<T>) -> Self {
        Self {
            map: NumericKeySortedVec {
                data: Default::default(),
                deleted: BitVec::new(),
                deleted_count: 0,
            },
            histogram: Histogram::new(HISTOGRAM_MAX_BUCKET_SIZE, HISTOGRAM_PRECISION),
            points_count: 0,
            max_values_per_point: 1,
            point_to_values: Default::default(),
            storage: Storage::Mmap(Box::new(index)),
        }
    }

    /// Load storage
    ///
    /// Loads in-memory index from backing RocksDB or mmap storage.
    pub(super) fn load(&mut self) -> OperationResult<bool> {
        match self.storage {
            #[cfg(feature = "rocksdb")]
            Storage::RocksDb(_) => self.load_rocksdb(),
            Storage::Mmap(_) => self.load_mmap(),
        }
    }

    /// Load from RocksDB storage
    ///
    /// Loads in-memory index from RocksDB storage.
    #[cfg(feature = "rocksdb")]
    fn load_rocksdb(&mut self) -> OperationResult<bool> {
        use super::mutable_numeric_index::MutableNumericIndex;

        let Storage::RocksDb(db_wrapper) = &self.storage else {
            return Err(OperationError::service_error(
                "Failed to load index from RocksDB, using different storage backend",
            ));
        };

        let mut mutable = MutableNumericIndex::<T>::open_rocksdb_db_wrapper(db_wrapper.clone());
        mutable.load()?;
        let InMemoryNumericIndex {
            map,
            histogram,
            points_count,
            max_values_per_point,
            point_to_values,
        } = mutable.into_in_memory_index();

        self.map = NumericKeySortedVec::from_btree_set(map);
        self.histogram = histogram;
        self.points_count = points_count;
        self.max_values_per_point = max_values_per_point;
        self.point_to_values = ImmutablePointToValues::new(point_to_values);

        Ok(true)
    }

    /// Load from mmap storage
    ///
    /// Loads in-memory index from mmap storage.
    fn load_mmap(&mut self) -> OperationResult<bool> {
        #[allow(irrefutable_let_patterns)]
        let Storage::Mmap(index) = &self.storage else {
            return Err(OperationError::service_error(
                "Failed to load index from mmap, using different storage backend",
            ));
        };

        let InMemoryNumericIndex {
            map,
            histogram,
            points_count,
            max_values_per_point,
            point_to_values,
        } = InMemoryNumericIndex::from_mmap(index);

        // Index is now loaded into memory, clear cache of backing mmap storage
        if let Err(err) = index.clear_cache() {
            log::warn!("Failed to clear mmap cache of ram mmap numeric index: {err}");
        }

        self.map = NumericKeySortedVec::from_btree_set(map);
        self.histogram = histogram;
        self.points_count = points_count;
        self.max_values_per_point = max_values_per_point;
        self.point_to_values = ImmutablePointToValues::new(point_to_values);

        Ok(true)
    }

    #[cfg(all(test, feature = "rocksdb"))]
    pub(super) fn db_wrapper(&self) -> Option<&DatabaseColumnScheduledDeleteWrapper> {
        match &self.storage {
            #[cfg(feature = "rocksdb")]
            Storage::RocksDb(db_wrapper) => Some(db_wrapper),
            Storage::Mmap(_) => None,
        }
    }

    #[inline]
    pub(super) fn wipe(self) -> OperationResult<()> {
        match self.storage {
            #[cfg(feature = "rocksdb")]
            Storage::RocksDb(db_wrapper) => db_wrapper.recreate_column_family(),
            Storage::Mmap(index) => index.wipe(),
        }
    }

    /// Clear cache
    ///
    /// Only clears cache of mmap storage if used. Does not clear in-memory representation of
    /// index.
    pub fn clear_cache(&self) -> OperationResult<()> {
        match &self.storage {
            #[cfg(feature = "rocksdb")]
            Storage::RocksDb(_) => Ok(()),
            Storage::Mmap(index) => index.clear_cache(),
        }
    }

    #[inline]
    pub(super) fn files(&self) -> Vec<PathBuf> {
        match &self.storage {
            #[cfg(feature = "rocksdb")]
            Storage::RocksDb(_) => vec![],
            Storage::Mmap(index) => index.files(),
        }
    }

    #[inline]
    pub(super) fn immutable_files(&self) -> Vec<PathBuf> {
        match &self.storage {
            #[cfg(feature = "rocksdb")]
            Storage::RocksDb(_) => vec![],
            Storage::Mmap(index) => index.immutable_files(),
        }
    }

    #[inline]
    pub(super) fn flusher(&self) -> Flusher {
        match &self.storage {
            #[cfg(feature = "rocksdb")]
            Storage::RocksDb(db_wrapper) => db_wrapper.flusher(),
            Storage::Mmap(index) => index.flusher(),
        }
    }

    pub(super) fn check_values_any(
        &self,
        idx: PointOffsetType,
        check_fn: impl Fn(&T) -> bool,
    ) -> bool {
        self.point_to_values.check_values_any(idx, |v| check_fn(v))
    }

    pub fn get_values(&self, idx: PointOffsetType) -> Option<Box<dyn Iterator<Item = T> + '_>> {
        Some(Box::new(
            self.point_to_values
                .get_values(idx)
                .map(|iter| iter.copied())?,
        ))
    }

    pub fn values_count(&self, idx: PointOffsetType) -> Option<usize> {
        self.point_to_values.get_values_count(idx)
    }

    pub(super) fn total_unique_values_count(&self) -> usize {
        self.map.len()
    }

    pub(super) fn values_range_size(
        &self,
        start_bound: Bound<Point<T>>,
        end_bound: Bound<Point<T>>,
    ) -> usize {
        let iterator = self.map.values_range(start_bound, end_bound);
        iterator.end_index - iterator.start_index
    }

    pub(super) fn values_range(
        &self,
        start_bound: Bound<Point<T>>,
        end_bound: Bound<Point<T>>,
    ) -> impl Iterator<Item = PointOffsetType> {
        self.map
            .values_range(start_bound, end_bound)
            .map(|Point { idx, .. }| idx)
    }

    pub(super) fn orderable_values_range(
        &self,
        start_bound: Bound<Point<T>>,
        end_bound: Bound<Point<T>>,
    ) -> impl DoubleEndedIterator<Item = (T, PointOffsetType)> + '_ {
        self.map
            .values_range(start_bound, end_bound)
            .map(|Point { val, idx, .. }| (val, idx))
    }

    #[cfg_attr(not(feature = "rocksdb"), expect(clippy::unnecessary_wraps))]
    pub(super) fn remove_point(&mut self, idx: PointOffsetType) -> OperationResult<()> {
        if let Some(removed_values) = self.point_to_values.get_values(idx) {
            let mut removed_count = 0;
            for value in removed_values {
                let key = Point::new(*value, idx);
                Self::remove_from_map(&mut self.map, &mut self.histogram, &key);

                // Update persisted storage
                match &mut self.storage {
                    #[cfg(feature = "rocksdb")]
                    Storage::RocksDb(db_wrapper) => {
                        let encoded = value.encode_key(idx);
                        db_wrapper.remove(encoded)?;
                    }
                    Storage::Mmap(index) => {
                        index.remove_point(idx);
                    }
                }

                removed_count += 1;
            }
            if removed_count > 0 {
                self.points_count -= 1;
            }
        }
        self.point_to_values.remove_point(idx);
        Ok(())
    }

    pub(super) fn get_histogram(&self) -> &Histogram<T> {
        &self.histogram
    }

    pub(super) fn get_points_count(&self) -> usize {
        self.points_count
    }

    pub(super) fn get_max_values_per_point(&self) -> usize {
        self.max_values_per_point
    }

    fn remove_from_map(
        map: &mut NumericKeySortedVec<T>,
        histogram: &mut Histogram<T>,
        key: &Point<T>,
    ) {
        if map.remove(key) {
            histogram.remove(
                key,
                |x| Self::get_histogram_left_neighbor(map, x),
                |x| Self::get_histogram_right_neighbor(map, x),
            );
        }
    }

    fn get_histogram_left_neighbor(
        map: &NumericKeySortedVec<T>,
        point: &Point<T>,
    ) -> Option<Point<T>> {
        map.values_range(Bound::Unbounded, Bound::Excluded(point.clone()))
            .next_back()
    }

    fn get_histogram_right_neighbor(
        map: &NumericKeySortedVec<T>,
        point: &Point<T>,
    ) -> Option<Point<T>> {
        map.values_range(Bound::Excluded(point.clone()), Bound::Unbounded)
            .next()
    }

    pub fn storage_type(&self) -> StorageType {
        match &self.storage {
            #[cfg(feature = "rocksdb")]
            Storage::RocksDb(_) => StorageType::RocksDb,
            Storage::Mmap(index) => StorageType::Mmap {
                is_on_disk: index.is_on_disk(),
            },
        }
    }

    #[cfg(feature = "rocksdb")]
    pub fn is_rocksdb(&self) -> bool {
        match self.storage {
            Storage::RocksDb(_) => true,
            Storage::Mmap(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use super::*;
    use crate::types::FloatPayloadType;

    fn check_range(
        key_set: &NumericKeySortedVec<FloatPayloadType>,
        encoded_map: &BTreeSet<Point<FloatPayloadType>>,
        start_bound: Bound<Point<FloatPayloadType>>,
        end_bound: Bound<Point<FloatPayloadType>>,
    ) {
        let set1 = key_set
            .values_range(start_bound.clone(), end_bound.clone())
            .collect::<Vec<_>>();

        let set2 = encoded_map
            .range((start_bound, end_bound))
            .cloned()
            .collect::<Vec<_>>();

        for (k1, k2) in set1.iter().zip(set2.iter()) {
            assert_eq!(k1, k2);
        }
    }

    fn check_ranges(
        key_set: &NumericKeySortedVec<FloatPayloadType>,
        encoded_map: &BTreeSet<Point<FloatPayloadType>>,
    ) {
        check_range(key_set, encoded_map, Bound::Unbounded, Bound::Unbounded);
        check_range(
            key_set,
            encoded_map,
            Bound::Unbounded,
            Bound::Included(Point::new(0.4, 2)),
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Unbounded,
            Bound::Excluded(Point::new(0.4, 2)),
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Included(Point::new(0.4, 2)),
            Bound::Unbounded,
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Excluded(Point::new(0.4, 2)),
            Bound::Unbounded,
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Included(Point::new(-5.0, 1)),
            Bound::Included(Point::new(5.0, 1)),
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Included(Point::new(-5.0, 1)),
            Bound::Excluded(Point::new(5.0, 1)),
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Excluded(Point::new(-5.0, 1)),
            Bound::Included(Point::new(5.0, 1)),
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Excluded(Point::new(-5.0, 1)),
            Bound::Excluded(Point::new(5.0, 1)),
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Included(Point::new(-5.0, 1000)),
            Bound::Included(Point::new(5.0, 1000)),
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Excluded(Point::new(-5.0, 1000)),
            Bound::Excluded(Point::new(5.0, 1000)),
        );
        check_range(
            key_set,
            encoded_map,
            Bound::Excluded(Point::new(-50000.0, 1000)),
            Bound::Excluded(Point::new(50000.0, 1000)),
        );
    }

    #[test]
    fn test_numeric_index_key_set() {
        let pairs = [
            Point::new(0.0, 1),
            Point::new(0.0, 3),
            Point::new(-0.0, 2),
            Point::new(-0.0, 4),
            Point::new(0.4, 2),
            Point::new(-0.4, 3),
            Point::new(5.0, 1),
            Point::new(-5.0, 1),
            Point::new(f64::INFINITY, 0),
            Point::new(f64::NEG_INFINITY, 1),
            Point::new(f64::NEG_INFINITY, 2),
            Point::new(f64::NEG_INFINITY, 3),
        ];

        let mut set_byte: BTreeSet<Point<FloatPayloadType>> = pairs.iter().cloned().collect();
        let mut set_keys =
            NumericKeySortedVec::<FloatPayloadType>::from_btree_set(set_byte.clone());

        check_ranges(&set_keys, &set_byte);

        // test deletion and ranges after deletion
        let deleted_key = Point::new(0.4, 2);
        set_keys.remove(&deleted_key);
        set_byte.remove(&deleted_key);

        check_ranges(&set_keys, &set_byte);

        // test deletion and ranges after deletion
        let deleted_key = Point::new(-5.0, 1);
        set_keys.remove(&deleted_key);
        set_byte.remove(&deleted_key);

        check_ranges(&set_keys, &set_byte);
    }
}
