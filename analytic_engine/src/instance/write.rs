// Copyright 2022-2023 CeresDB Project Authors. Licensed under Apache-2.0.

//! Write logic of instance

use ceresdbproto::{schema as schema_pb, table_requests};
use common_types::{
    bytes::ByteVec,
    row::{RowGroup, RowGroupSlicer},
    schema::{IndexInWriterSchema, Schema},
};
use common_util::{codec::row, define_result};
use log::{debug, error, info, trace, warn};
use smallvec::SmallVec;
use snafu::{ensure, Backtrace, ResultExt, Snafu};
use table_engine::table::WriteRequest;
use wal::{
    kv_encoder::LogBatchEncoder,
    manager::{SequenceNumber, WalLocation, WriteContext},
};

use crate::{
    instance,
    instance::{
        flush_compaction::TableFlushOptions, serial_executor::TableOpSerialExecutor, InstanceRef,
    },
    memtable::{key::KeySequence, PutContext},
    payload::WritePayload,
    space::{SpaceAndTable, SpaceRef},
    table::{data::TableDataRef, version::MemTableForWrite},
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display(
        "Failed to encode payloads, table:{}, wal_location:{:?}, err:{}",
        table,
        wal_location,
        source
    ))]
    EncodePayloads {
        table: String,
        wal_location: WalLocation,
        source: wal::manager::Error,
    },

    #[snafu(display("Failed to write to wal, table:{}, err:{}", table, source))]
    WriteLogBatch {
        table: String,
        source: wal::manager::Error,
    },

    #[snafu(display("Failed to write to memtable, table:{}, err:{}", table, source))]
    WriteMemTable {
        table: String,
        source: crate::table::version::Error,
    },

    #[snafu(display("Try to write to a dropped table, table:{}", table))]
    WriteDroppedTable { table: String },

    #[snafu(display(
        "Too many rows to write (more than {}), table:{}, rows:{}.\nBacktrace:\n{}",
        MAX_ROWS_TO_WRITE,
        table,
        rows,
        backtrace,
    ))]
    TooManyRows {
        table: String,
        rows: usize,
        backtrace: Backtrace,
    },

    #[snafu(display("Failed to find mutable memtable, table:{}, err:{}", table, source))]
    FindMutableMemTable {
        table: String,
        source: crate::table::data::Error,
    },

    #[snafu(display("Failed to flush table, table:{}, err:{}", table, source))]
    FlushTable {
        table: String,
        source: crate::instance::flush_compaction::Error,
    },

    #[snafu(display(
        "Background flush failed, cannot write more data, err:{}.\nBacktrace:\n{}",
        msg,
        backtrace
    ))]
    BackgroundFlushFailed { msg: String, backtrace: Backtrace },

    #[snafu(display("Schema of request is incompatible with table, err:{}", source))]
    IncompatSchema {
        source: common_types::schema::CompatError,
    },

    #[snafu(display("Failed to encode row group, err:{}", source))]
    EncodeRowGroup {
        source: common_util::codec::row::Error,
    },

    #[snafu(display("Failed to update sequence of memtable, err:{}", source))]
    UpdateMemTableSequence { source: crate::memtable::Error },
}

define_result!(Error);

/// Max rows in a write request, must less than [u32::MAX]
const MAX_ROWS_TO_WRITE: usize = 10_000_000;

pub(crate) struct EncodeContext {
    pub row_group: RowGroup,
    pub index_in_writer: IndexInWriterSchema,
    pub encoded_rows: Vec<ByteVec>,
}

impl EncodeContext {
    pub fn new(row_group: RowGroup) -> Self {
        Self {
            row_group,
            index_in_writer: IndexInWriterSchema::default(),
            encoded_rows: Vec::new(),
        }
    }

    pub fn encode_rows(&mut self, table_schema: &Schema) -> Result<()> {
        row::encode_row_group_for_wal(
            &self.row_group,
            table_schema,
            &self.index_in_writer,
            &mut self.encoded_rows,
        )
        .context(EncodeRowGroup)?;

        assert_eq!(self.row_group.num_rows(), self.encoded_rows.len());

        Ok(())
    }
}

/// Split the write request into multiple batches whose size is determined by
/// the `max_bytes_per_batch`.
struct WriteRowGroupSplitter {
    /// Max bytes per batch. Actually, the size of a batch is not exactly
    /// ensured less than this `max_bytes_per_batch`, but it is guaranteed that
    /// the batch contains at most one more row when its size exceeds this
    /// `max_bytes_per_batch`.
    max_bytes_per_batch: usize,
}

enum SplitResult<'a> {
    Splitted {
        encoded_batches: Vec<Vec<ByteVec>>,
        row_group_batches: Vec<RowGroupSlicer<'a>>,
    },
    Integrate {
        encoded_rows: Vec<ByteVec>,
        row_group: RowGroupSlicer<'a>,
    },
}

impl WriteRowGroupSplitter {
    pub fn new(max_bytes_per_batch: usize) -> Self {
        Self {
            max_bytes_per_batch,
        }
    }

    /// Split the write request into multiple batches.
    ///
    /// NOTE: The length of the `encoded_rows` should be the same as the number
    /// of rows in the `row_group`.
    pub fn split<'a>(
        &'_ self,
        encoded_rows: Vec<ByteVec>,
        row_group: &'a RowGroup,
    ) -> SplitResult<'a> {
        let end_row_indexes = self.compute_batches(&encoded_rows);
        if end_row_indexes.len() <= 1 {
            // No need to split.
            return SplitResult::Integrate {
                encoded_rows,
                row_group: RowGroupSlicer::from(row_group),
            };
        }

        let mut prev_end_row_index = 0;
        let mut encoded_batches = Vec::with_capacity(end_row_indexes.len());
        let mut row_group_batches = Vec::with_capacity(end_row_indexes.len());
        for end_row_index in &end_row_indexes {
            let end_row_index = *end_row_index;
            let curr_batch = Vec::with_capacity(end_row_index - prev_end_row_index);
            encoded_batches.push(curr_batch);
            let row_group_slicer =
                RowGroupSlicer::new(prev_end_row_index..end_row_index, row_group);
            row_group_batches.push(row_group_slicer);

            prev_end_row_index = end_row_index;
        }

        let mut current_batch_idx = 0;
        for (row_idx, encoded_row) in encoded_rows.into_iter().enumerate() {
            if row_idx >= end_row_indexes[current_batch_idx] {
                current_batch_idx += 1;
            }
            encoded_batches[current_batch_idx].push(encoded_row);
        }

        SplitResult::Splitted {
            encoded_batches,
            row_group_batches,
        }
    }

    /// Compute the end row indexes in the original `encoded_rows` of each
    /// batch.
    fn compute_batches(&self, encoded_rows: &[ByteVec]) -> Vec<usize> {
        let mut current_batch_size = 0;
        let mut end_row_indexes = Vec::new();
        for (row_idx, encoded_row) in encoded_rows.iter().enumerate() {
            let row_size = encoded_row.len();
            current_batch_size += row_size;

            // If the current batch size exceeds the `max_bytes_per_batch`, freeze this
            // batch by recording its end row index.
            // Note that such check may cause the batch size exceeds the
            // `max_bytes_per_batch`.
            if current_batch_size >= self.max_bytes_per_batch {
                current_batch_size = 0;
                end_row_indexes.push(row_idx + 1)
            }
        }

        if current_batch_size > 0 {
            end_row_indexes.push(encoded_rows.len());
        }

        end_row_indexes
    }
}

pub struct Writer<'a> {
    instance: InstanceRef,
    space: SpaceRef,
    table_data: TableDataRef,
    serial_exec: &'a mut TableOpSerialExecutor,
}

impl<'a> Writer<'a> {
    pub fn new(
        instance: InstanceRef,
        space_table: SpaceAndTable,
        serial_exec: &'a mut TableOpSerialExecutor,
    ) -> Writer<'a> {
        assert_eq!(space_table.table_data().id, serial_exec.table_id());

        Self {
            instance,
            space: space_table.space().clone(),
            table_data: space_table.table_data().clone(),
            serial_exec,
        }
    }
}

pub(crate) struct MemTableWriter<'a> {
    table_data: TableDataRef,
    _serial_exec: &'a mut TableOpSerialExecutor,
}

impl<'a> MemTableWriter<'a> {
    pub fn new(table_data: TableDataRef, serial_exec: &'a mut TableOpSerialExecutor) -> Self {
        Self {
            table_data,
            _serial_exec: serial_exec,
        }
    }

    // TODO(yingwen): How to trigger flush if we found memtables are full during
    // inserting memtable? RocksDB checks memtable size in MemTableInserter
    /// Write data into memtable.
    ///
    /// index_in_writer must match the schema in table_data.
    pub fn write(
        &self,
        sequence: SequenceNumber,
        row_group: &RowGroupSlicer,
        index_in_writer: IndexInWriterSchema,
    ) -> Result<()> {
        let _timer = self.table_data.metrics.start_table_write_memtable_timer();
        if row_group.is_empty() {
            return Ok(());
        }

        let schema = &self.table_data.schema();
        // Store all memtables we wrote and update their last sequence later.
        let mut wrote_memtables: SmallVec<[_; 4]> = SmallVec::new();
        let mut last_mutable_mem: Option<MemTableForWrite> = None;

        let mut ctx = PutContext::new(index_in_writer);
        for (row_idx, row) in row_group.iter().enumerate() {
            // TODO(yingwen): Add RowWithSchema and take RowWithSchema as input, then remove
            // this unwrap()
            let timestamp = row.timestamp(schema).unwrap();
            // skip expired row
            if self.table_data.is_expired(timestamp) {
                trace!("Skip expired row when write to memtable, row:{:?}", row);
                continue;
            }
            if last_mutable_mem.is_none()
                || !last_mutable_mem
                    .as_ref()
                    .unwrap()
                    .accept_timestamp(timestamp)
            {
                // The time range is not processed by current memtable, find next one.
                let mutable_mem = self
                    .table_data
                    .find_or_create_mutable(timestamp, schema)
                    .context(FindMutableMemTable {
                        table: &self.table_data.name,
                    })?;
                wrote_memtables.push(mutable_mem.clone());
                last_mutable_mem = Some(mutable_mem);
            }

            // We have check the row num is less than `MAX_ROWS_TO_WRITE`, it is safe to
            // cast it to u32 here
            let key_seq = KeySequence::new(sequence, row_idx as u32);
            // TODO(yingwen): Batch sample timestamp in sampling phase.
            last_mutable_mem
                .as_ref()
                .unwrap()
                .put(&mut ctx, key_seq, row, schema, timestamp)
                .context(WriteMemTable {
                    table: &self.table_data.name,
                })?;
        }

        // Update last sequence of memtable.
        for mem_wrote in wrote_memtables {
            mem_wrote
                .set_last_sequence(sequence)
                .context(UpdateMemTableSequence)?;
        }

        Ok(())
    }
}

impl<'a> Writer<'a> {
    pub(crate) async fn write(&mut self, request: WriteRequest) -> Result<usize> {
        let _timer = self.table_data.metrics.start_table_write_execute_timer();
        self.table_data.metrics.on_write_request_begin();

        self.validate_before_write(&request)?;
        let mut encode_ctx = EncodeContext::new(request.row_group);

        self.preprocess_write(&mut encode_ctx).await?;

        {
            let _timer = self.table_data.metrics.start_table_write_encode_timer();
            let schema = self.table_data.schema();
            encode_ctx.encode_rows(&schema)?;
        }

        let EncodeContext {
            row_group,
            index_in_writer,
            encoded_rows,
        } = encode_ctx;

        let table_data = self.table_data.clone();
        let split_res = self.maybe_split_write_request(encoded_rows, &row_group);
        match split_res {
            SplitResult::Integrate {
                encoded_rows,
                row_group,
            } => {
                self.write_table_row_group(&table_data, row_group, index_in_writer, encoded_rows)
                    .await?;
            }
            SplitResult::Splitted {
                encoded_batches,
                row_group_batches,
            } => {
                for (encoded_rows, row_group) in encoded_batches.into_iter().zip(row_group_batches)
                {
                    self.write_table_row_group(
                        &table_data,
                        row_group,
                        index_in_writer.clone(),
                        encoded_rows,
                    )
                    .await?;
                }
            }
        }

        Ok(row_group.num_rows())
    }

    fn maybe_split_write_request<'b>(
        &'a self,
        encoded_rows: Vec<ByteVec>,
        row_group: &'b RowGroup,
    ) -> SplitResult<'b> {
        if self.instance.max_bytes_per_write_batch.is_none() {
            return SplitResult::Integrate {
                encoded_rows,
                row_group: RowGroupSlicer::from(row_group),
            };
        }

        let splitter = WriteRowGroupSplitter::new(self.instance.max_bytes_per_write_batch.unwrap());
        splitter.split(encoded_rows, row_group)
    }

    async fn write_table_row_group(
        &mut self,
        table_data: &TableDataRef,
        row_group: RowGroupSlicer<'_>,
        index_in_writer: IndexInWriterSchema,
        encoded_rows: Vec<ByteVec>,
    ) -> Result<()> {
        let sequence = self.write_to_wal(encoded_rows).await?;
        let memtable_writer = MemTableWriter::new(table_data.clone(), self.serial_exec);

        memtable_writer
            .write(sequence, &row_group, index_in_writer)
            .map_err(|e| {
                error!(
                    "Failed to write to memtable, table:{}, table_id:{}, err:{}",
                    table_data.name, table_data.id, e
                );
                e
            })?;

        // Failure of writing memtable may cause inconsecutive sequence.
        if table_data.last_sequence() + 1 != sequence {
            warn!(
                "Sequence must be consecutive, table:{}, table_id:{}, last_sequence:{}, wal_sequence:{}",
                table_data.name,table_data.id,
                table_data.last_sequence(),
                sequence
            );
        }

        debug!(
            "Instance write finished, update sequence, table:{}, table_id:{} last_sequence:{}",
            table_data.name, table_data.id, sequence
        );

        table_data.set_last_sequence(sequence);

        // Collect metrics.
        table_data
            .metrics
            .on_write_request_done(row_group.num_rows());

        Ok(())
    }

    /// Return Ok if the request is valid, this is done before entering the
    /// write thread.
    fn validate_before_write(&self, request: &WriteRequest) -> Result<()> {
        ensure!(
            request.row_group.num_rows() < MAX_ROWS_TO_WRITE,
            TooManyRows {
                table: &self.table_data.name,
                rows: request.row_group.num_rows(),
            }
        );

        Ok(())
    }

    /// Preprocess before write, check:
    ///  - whether table is dropped
    ///  - memtable capacity and maybe trigger flush
    ///
    /// Fills [common_types::schema::IndexInWriterSchema] in [EncodeContext]
    async fn preprocess_write(&mut self, encode_ctx: &mut EncodeContext) -> Result<()> {
        let _total_timer = self.table_data.metrics.start_table_write_preprocess_timer();
        ensure!(
            !self.table_data.is_dropped(),
            WriteDroppedTable {
                table: &self.table_data.name,
            }
        );

        // Checks schema compatibility.
        self.table_data
            .schema()
            .compatible_for_write(
                encode_ctx.row_group.schema(),
                &mut encode_ctx.index_in_writer,
            )
            .context(IncompatSchema)?;

        if self.instance.should_flush_instance() {
            if let Some(space) = self.instance.space_store.find_maximum_memory_usage_space() {
                if let Some(table) = space.find_maximum_memory_usage_table() {
                    info!("Trying to flush table {} bytes {} in space {} because engine total memtable memory usage exceeds db_write_buffer_size {}.",
                          table.name,
                          table.memtable_memory_usage(),
                          space.id,
                          self.instance.db_write_buffer_size,
                    );
                    let _timer = self
                        .table_data
                        .metrics
                        .start_table_write_instance_flush_wait_timer();
                    self.handle_memtable_flush(&table).await?;
                }
            }
        }

        if self.space.should_flush_space() {
            if let Some(table) = self.space.find_maximum_memory_usage_table() {
                info!("Trying to flush table {} bytes {} in space {} because space total memtable memory usage exceeds space_write_buffer_size {}.",
                      table.name,
                      table.memtable_memory_usage() ,
                      self.space.id,
                      self.space.write_buffer_size,
                );
                let _timer = self
                    .table_data
                    .metrics
                    .start_table_write_space_flush_wait_timer();
                self.handle_memtable_flush(&table).await?;
            }
        }

        if self.table_data.should_flush_table(self.serial_exec) {
            let table_data = self.table_data.clone();
            let _timer = table_data.metrics.start_table_write_flush_wait_timer();
            self.handle_memtable_flush(&table_data).await?;
        }

        Ok(())
    }

    /// Write log_batch into wal, return the sequence number of log_batch.
    async fn write_to_wal(&self, encoded_rows: Vec<ByteVec>) -> Result<SequenceNumber> {
        let _timer = self.table_data.metrics.start_table_write_wal_timer();
        // Convert into pb
        let write_req_pb = table_requests::WriteRequest {
            // FIXME: Shall we avoid the magic number here?
            version: 0,
            // Use the table schema instead of the schema in request to avoid schema
            // mismatch during replaying
            schema: Some(schema_pb::TableSchema::from(&self.table_data.schema())),
            rows: encoded_rows,
        };

        // Encode payload
        let payload = WritePayload::Write(&write_req_pb);
        let table_location = self.table_data.table_location();
        let wal_location =
            instance::create_wal_location(table_location.id, table_location.shard_info);
        let log_batch_encoder = LogBatchEncoder::create(wal_location);
        let log_batch = log_batch_encoder.encode(&payload).context(EncodePayloads {
            table: &self.table_data.name,
            wal_location,
        })?;

        // Write to wal manager
        let write_ctx = WriteContext::default();
        let sequence = self
            .instance
            .space_store
            .wal_manager
            .write(&write_ctx, &log_batch)
            .await
            .context(WriteLogBatch {
                table: &self.table_data.name,
            })?;

        Ok(sequence)
    }

    /// Flush memtables of table in background.
    ///
    /// Note the table to flush may not the same as `self.table_data`. And if we
    /// try to flush other table in this table's writer, the lock should be
    /// acquired in advance. And in order to avoid deadlock, we should not wait
    /// for the lock.
    async fn handle_memtable_flush(&mut self, table_data: &TableDataRef) -> Result<()> {
        let opts = TableFlushOptions {
            res_sender: None,
            max_retry_flush_limit: self.instance.max_retry_flush_limit(),
        };
        let flusher = self.instance.make_flusher();
        if table_data.id == self.table_data.id {
            let flush_scheduler = self.serial_exec.flush_scheduler();
            // Set `block_on_write_thread` to false and let flush do in background.
            return flusher
                .schedule_flush(flush_scheduler, table_data, opts)
                .await
                .context(FlushTable {
                    table: &table_data.name,
                });
        }

        debug!(
            "Try to trigger flush of other table:{} from the write procedure of table:{}",
            table_data.name, self.table_data.name
        );
        match table_data.serial_exec.try_lock() {
            Ok(mut serial_exec) => {
                let flush_scheduler = serial_exec.flush_scheduler();
                // Set `block_on_write_thread` to false and let flush do in background.
                flusher
                    .schedule_flush(flush_scheduler, table_data, opts)
                    .await
                    .context(FlushTable {
                        table: &table_data.name,
                    })
            }
            Err(_) => {
                warn!(
                    "Failed to acquire write lock for flush table:{}",
                    table_data.name,
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use common_types::{
        column_schema::Builder as ColumnSchemaBuilder,
        datum::{Datum, DatumKind},
        row::{Row, RowGroupBuilder},
        schema::Builder as SchemaBuilder,
        time::Timestamp,
    };

    use super::*;

    fn generate_rows_for_test(sizes: Vec<usize>) -> (Vec<ByteVec>, RowGroup) {
        let encoded_rows: Vec<_> = sizes.iter().map(|size| vec![0; *size]).collect();
        let rows: Vec<_> = sizes
            .iter()
            .map(|size| {
                let datum = Datum::Timestamp(Timestamp::new(*size as i64));
                Row::from_datums(vec![datum])
            })
            .collect();

        let column_schema = ColumnSchemaBuilder::new("ts".to_string(), DatumKind::Timestamp)
            .build()
            .unwrap();
        let schema = SchemaBuilder::new()
            .add_key_column(column_schema)
            .unwrap()
            .build()
            .unwrap();
        let row_group = RowGroupBuilder::with_rows(schema, rows).unwrap().build();

        (encoded_rows, row_group)
    }

    #[test]
    fn test_write_split_compute_batches() {
        let cases = vec![
            (2, vec![1, 2, 3, 4, 5], vec![2, 3, 4, 5]),
            (100, vec![50, 50, 100, 10], vec![2, 3, 4]),
            (1000, vec![50, 50, 100, 10], vec![4]),
            (2, vec![10, 10, 0, 10], vec![1, 2, 4]),
            (0, vec![10, 10, 0, 10], vec![1, 2, 3, 4]),
            (0, vec![0, 0], vec![1, 2]),
            (10, vec![], vec![]),
        ];
        for (batch_size, sizes, expected_batch_indexes) in cases {
            let (encoded_rows, _) = generate_rows_for_test(sizes);
            let write_row_group_splitter = WriteRowGroupSplitter::new(batch_size);
            let batch_indexes = write_row_group_splitter.compute_batches(&encoded_rows);
            assert_eq!(batch_indexes, expected_batch_indexes);
        }
    }

    #[test]
    fn test_write_split_row_group() {
        let cases = vec![
            (
                2,
                vec![1, 2, 3, 4, 5],
                vec![vec![1, 2], vec![3], vec![4], vec![5]],
            ),
            (
                100,
                vec![50, 50, 100, 10],
                vec![vec![50, 50], vec![100], vec![10]],
            ),
            (1000, vec![50, 50, 100, 10], vec![vec![50, 50, 100, 10]]),
            (
                2,
                vec![10, 10, 0, 10],
                vec![vec![10], vec![10], vec![0, 10]],
            ),
            (
                0,
                vec![10, 10, 0, 10],
                vec![vec![10], vec![10], vec![0], vec![10]],
            ),
            (0, vec![0, 0], vec![vec![0], vec![0]]),
            (10, vec![], vec![]),
        ];

        let check_encoded_rows = |encoded_rows: &[ByteVec], expected_row_sizes: &[usize]| {
            assert_eq!(encoded_rows.len(), expected_row_sizes.len());
            for (encoded_row, expected_row_size) in
                encoded_rows.iter().zip(expected_row_sizes.iter())
            {
                assert_eq!(encoded_row.len(), *expected_row_size);
            }
        };
        for (batch_size, sizes, expected_batches) in cases {
            let (encoded_rows, row_group) = generate_rows_for_test(sizes.clone());
            let write_row_group_splitter = WriteRowGroupSplitter::new(batch_size);
            let split_res = write_row_group_splitter.split(encoded_rows, &row_group);
            if expected_batches.is_empty() {
                assert!(matches!(split_res, SplitResult::Integrate { .. }));
            } else if expected_batches.len() == 1 {
                assert!(matches!(split_res, SplitResult::Integrate { .. }));
                if let SplitResult::Integrate {
                    encoded_rows,
                    row_group,
                } = split_res
                {
                    check_encoded_rows(&encoded_rows, &expected_batches[0]);
                    assert_eq!(row_group.num_rows(), expected_batches[0].len());
                }
            } else {
                assert!(matches!(split_res, SplitResult::Splitted { .. }));
                if let SplitResult::Splitted {
                    encoded_batches,
                    row_group_batches,
                } = split_res
                {
                    assert_eq!(encoded_batches.len(), row_group_batches.len());
                    assert_eq!(encoded_batches.len(), expected_batches.len());
                    let mut batch_start_index = 0;
                    for ((encoded_batch, row_group_batch), expected_batch) in encoded_batches
                        .iter()
                        .zip(row_group_batches.iter())
                        .zip(expected_batches.iter())
                    {
                        check_encoded_rows(encoded_batch, expected_batch);
                        assert_eq!(row_group_batch.num_rows(), expected_batch.len());
                        assert_eq!(row_group_batch.slice_range().start, batch_start_index);
                        batch_start_index += expected_batch.len();
                    }
                }
            }
        }
    }
}
