mod extract;
mod helpers;
mod transform;
mod typed_chunk;

use std::collections::HashSet;
use std::io::{Cursor, Read, Seek};
use std::iter::FromIterator;
use std::num::{NonZeroU32, NonZeroUsize};

use crossbeam_channel::{Receiver, Sender};
use heed::types::Str;
use heed::Database;
use log::debug;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use slice_group_by::GroupBy;
use typed_chunk::{write_typed_chunk_into_index, TypedChunk};

pub use self::helpers::{
    as_cloneable_grenad, create_sorter, create_writer, fst_stream_into_hashset,
    fst_stream_into_vec, merge_cbo_roaring_bitmaps, merge_roaring_bitmaps,
    sorter_into_lmdb_database, valid_lmdb_key, write_into_lmdb_database, writer_into_reader,
    ClonableMmap, MergeFn,
};
use self::helpers::{grenad_obkv_into_chunks, GrenadParameters};
pub use self::transform::{Transform, TransformOutput};
use crate::documents::DocumentBatchReader;
pub use crate::update::index_documents::helpers::CursorClonableMmap;
use crate::update::{
    self, Facets, IndexerConfig, UpdateIndexingStep, WordPrefixDocids,
    WordPrefixPairProximityDocids, WordPrefixPositionDocids, WordsPrefixesFst,
};
use crate::{Index, Result, RoaringBitmapCodec, UserError};

static MERGED_DATABASE_COUNT: usize = 7;
static PREFIX_DATABASE_COUNT: usize = 5;
static TOTAL_POSTING_DATABASE_COUNT: usize = MERGED_DATABASE_COUNT + PREFIX_DATABASE_COUNT;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentAdditionResult {
    /// The number of documents that were indexed during the update
    pub indexed_documents: u64,
    /// The total number of documents in the index after the update
    pub number_of_documents: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum IndexDocumentsMethod {
    /// Replace the previous document with the new one,
    /// removing all the already known attributes.
    ReplaceDocuments,

    /// Merge the previous version of the document with the new version,
    /// replacing old attributes values with the new ones and add the new attributes.
    UpdateDocuments,
}

impl Default for IndexDocumentsMethod {
    fn default() -> Self {
        Self::ReplaceDocuments
    }
}

pub struct IndexDocuments<'t, 'u, 'i, 'a, F> {
    wtxn: &'t mut heed::RwTxn<'i, 'u>,
    index: &'i Index,
    config: IndexDocumentsConfig,
    indexer_config: &'a IndexerConfig,
    transform: Option<Transform<'a, 'i>>,
    progress: F,
    added_documents: u64,
}

#[derive(Default, Debug, Clone)]
pub struct IndexDocumentsConfig {
    pub facet_level_group_size: Option<NonZeroUsize>,
    pub facet_min_level_size: Option<NonZeroUsize>,
    pub words_prefix_threshold: Option<u32>,
    pub max_prefix_length: Option<usize>,
    pub words_positions_level_group_size: Option<NonZeroU32>,
    pub words_positions_min_level_size: Option<NonZeroU32>,
    pub update_method: IndexDocumentsMethod,
    pub autogenerate_docids: bool,
}

impl<'t, 'u, 'i, 'a, F> IndexDocuments<'t, 'u, 'i, 'a, F>
where
    F: Fn(UpdateIndexingStep) + Sync,
{
    pub fn new(
        wtxn: &'t mut heed::RwTxn<'i, 'u>,
        index: &'i Index,
        indexer_config: &'a IndexerConfig,
        config: IndexDocumentsConfig,
        progress: F,
    ) -> Result<IndexDocuments<'t, 'u, 'i, 'a, F>> {
        let transform = Some(Transform::new(
            wtxn,
            &index,
            indexer_config,
            config.update_method,
            config.autogenerate_docids,
        )?);

        Ok(IndexDocuments {
            transform,
            config,
            indexer_config,
            progress,
            wtxn,
            index,
            added_documents: 0,
        })
    }

    /// Adds a batch of documents to the current builder.
    ///
    /// Since the documents are progressively added to the writer, a failure will cause a stale
    /// builder, and the builder must be discarded.
    ///
    /// Returns the number of documents added to the builder.
    pub fn add_documents<R>(&mut self, reader: DocumentBatchReader<R>) -> Result<u64>
    where
        R: Read + Seek,
    {
        // Early return when there is no document to add
        if reader.is_empty() {
            return Ok(0);
        }

        let indexed_documents = self
            .transform
            .as_mut()
            .expect("Invalid document addition state")
            .read_documents(reader, self.wtxn, &self.progress)?
            as u64;

        self.added_documents += indexed_documents;

        Ok(indexed_documents)
    }

    #[logging_timer::time("IndexDocuments::{}")]
    pub fn execute(mut self) -> Result<DocumentAdditionResult> {
        if self.added_documents == 0 {
            let number_of_documents = self.index.number_of_documents(self.wtxn)?;
            return Ok(DocumentAdditionResult { indexed_documents: 0, number_of_documents });
        }
        let output = self
            .transform
            .take()
            .expect("Invalid document addition state")
            .output_from_sorter(self.wtxn, &self.progress)?;

        let new_facets = output.compute_real_facets(self.wtxn, self.index)?;
        self.index.put_faceted_fields(self.wtxn, &new_facets)?;

        let indexed_documents = output.documents_count as u64;
        let number_of_documents = self.execute_raw(output)?;

        Ok(DocumentAdditionResult { indexed_documents, number_of_documents })
    }

    /// Returns the total number of documents in the index after the update.
    #[logging_timer::time("IndexDocuments::{}")]
    pub fn execute_raw(self, output: TransformOutput) -> Result<u64>
    where
        F: Fn(UpdateIndexingStep) + Sync,
    {
        let TransformOutput {
            primary_key,
            fields_ids_map,
            field_distribution,
            external_documents_ids,
            new_documents_ids,
            replaced_documents_ids,
            documents_count,
            original_documents,
            flattened_documents,
        } = output;

        // The fields_ids_map is put back to the store now so the rest of the transaction sees an
        // up to date field map.
        self.index.put_fields_ids_map(self.wtxn, &fields_ids_map)?;

        let backup_pool;
        let pool = match self.indexer_config.thread_pool {
            Some(ref pool) => pool,
            #[cfg(not(test))]
            None => {
                // We initialize a bakcup pool with the default
                // settings if none have already been set.
                backup_pool = rayon::ThreadPoolBuilder::new().build()?;
                &backup_pool
            }
            #[cfg(test)]
            None => {
                // We initialize a bakcup pool with the default
                // settings if none have already been set.
                backup_pool = rayon::ThreadPoolBuilder::new().num_threads(1).build()?;
                &backup_pool
            }
        };

        let original_documents = grenad::Reader::new(original_documents)?;
        let flattened_documents = grenad::Reader::new(flattened_documents)?;

        // create LMDB writer channel
        let (lmdb_writer_sx, lmdb_writer_rx): (
            Sender<Result<TypedChunk>>,
            Receiver<Result<TypedChunk>>,
        ) = crossbeam_channel::unbounded();

        // get the primary key field id
        let primary_key_id = fields_ids_map.id(&primary_key).unwrap();

        // get searchable fields for word databases
        let searchable_fields =
            self.index.searchable_fields_ids(self.wtxn)?.map(HashSet::from_iter);
        // get filterable fields for facet databases
        let faceted_fields = self.index.faceted_fields_ids(self.wtxn)?;
        // get the fid of the `_geo.lat` and `_geo.lng` fields.
        let geo_fields_ids = match self.index.fields_ids_map(self.wtxn)?.id("_geo") {
            Some(gfid) => {
                let is_sortable = self.index.sortable_fields_ids(self.wtxn)?.contains(&gfid);
                let is_filterable = self.index.filterable_fields_ids(self.wtxn)?.contains(&gfid);
                // if `_geo` is faceted then we get the `lat` and `lng`
                if is_sortable || is_filterable {
                    let field_ids = self
                        .index
                        .fields_ids_map(self.wtxn)?
                        .insert("_geo.lat")
                        .zip(self.index.fields_ids_map(self.wtxn)?.insert("_geo.lng"))
                        .ok_or(UserError::AttributeLimitReached)?;
                    Some(field_ids)
                } else {
                    None
                }
            }
            None => None,
        };

        let stop_words = self.index.stop_words(self.wtxn)?;
        let exact_attributes = self.index.exact_attributes_ids(self.wtxn)?;

        // Run extraction pipeline in parallel.
        pool.install(|| {
            let params = GrenadParameters {
                chunk_compression_type: self.indexer_config.chunk_compression_type,
                chunk_compression_level: self.indexer_config.chunk_compression_level,
                max_memory: self.indexer_config.max_memory,
                max_nb_chunks: self.indexer_config.max_nb_chunks, // default value, may be chosen.
            };

            // split obkv file into several chunks
            let original_chunk_iter = grenad_obkv_into_chunks(
                original_documents,
                params.clone(),
                self.indexer_config.documents_chunk_size.unwrap_or(1024 * 1024 * 4), // 4MiB
            );

            // split obkv file into several chunks
            let flattened_chunk_iter = grenad_obkv_into_chunks(
                flattened_documents,
                params.clone(),
                self.indexer_config.documents_chunk_size.unwrap_or(1024 * 1024 * 4), // 4MiB
            );

            let result = original_chunk_iter
                .and_then(|original_chunk_iter| Ok((original_chunk_iter, flattened_chunk_iter?)))
                .map(|(original_chunk, flattened_chunk)| {
                    // extract all databases from the chunked obkv douments
                    extract::data_from_obkv_documents(
                        original_chunk,
                        flattened_chunk,
                        params,
                        lmdb_writer_sx.clone(),
                        searchable_fields,
                        faceted_fields,
                        primary_key_id,
                        geo_fields_ids,
                        stop_words,
                        self.indexer_config.max_positions_per_attributes,
                        exact_attributes,
                    )
                });

            if let Err(e) = result {
                let _ = lmdb_writer_sx.send(Err(e));
            }

            // needs to be droped to avoid channel waiting lock.
            drop(lmdb_writer_sx)
        });

        // We delete the documents that this document addition replaces. This way we are
        // able to simply insert all the documents even if they already exist in the database.
        if !replaced_documents_ids.is_empty() {
            let mut deletion_builder = update::DeleteDocuments::new(self.wtxn, self.index)?;
            debug!("documents to delete {:?}", replaced_documents_ids);
            deletion_builder.delete_documents(&replaced_documents_ids);
            let deleted_documents_count = deletion_builder.execute()?;
            debug!("{} documents actually deleted", deleted_documents_count.deleted_documents);
        }

        let index_documents_ids = self.index.documents_ids(self.wtxn)?;
        let index_is_empty = index_documents_ids.len() == 0;
        let mut final_documents_ids = RoaringBitmap::new();
        let mut word_pair_proximity_docids = None;
        let mut word_position_docids = None;
        let mut word_docids = None;
        let mut exact_word_docids = None;

        let mut databases_seen = 0;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        for result in lmdb_writer_rx {
            let typed_chunk = match result? {
                TypedChunk::WordDocids { word_docids_reader, exact_word_docids_reader } => {
                    let cloneable_chunk = unsafe { as_cloneable_grenad(&word_docids_reader)? };
                    word_docids = Some(cloneable_chunk);
                    let cloneable_chunk =
                        unsafe { as_cloneable_grenad(&exact_word_docids_reader)? };
                    exact_word_docids = Some(cloneable_chunk);
                    TypedChunk::WordDocids { word_docids_reader, exact_word_docids_reader }
                }
                TypedChunk::WordPairProximityDocids(chunk) => {
                    let cloneable_chunk = unsafe { as_cloneable_grenad(&chunk)? };
                    word_pair_proximity_docids = Some(cloneable_chunk);
                    TypedChunk::WordPairProximityDocids(chunk)
                }
                TypedChunk::WordPositionDocids(chunk) => {
                    let cloneable_chunk = unsafe { as_cloneable_grenad(&chunk)? };
                    word_position_docids = Some(cloneable_chunk);
                    TypedChunk::WordPositionDocids(chunk)
                }
                otherwise => otherwise,
            };

            let (docids, is_merged_database) =
                write_typed_chunk_into_index(typed_chunk, &self.index, self.wtxn, index_is_empty)?;
            if !docids.is_empty() {
                final_documents_ids |= docids;
                let documents_seen_count = final_documents_ids.len();
                (self.progress)(UpdateIndexingStep::IndexDocuments {
                    documents_seen: documents_seen_count as usize,
                    total_documents: documents_count,
                });
                debug!(
                    "We have seen {} documents on {} total document so far",
                    documents_seen_count, documents_count
                );
            }
            if is_merged_database {
                databases_seen += 1;
                (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
                    databases_seen,
                    total_databases: TOTAL_POSTING_DATABASE_COUNT,
                });
            }
        }

        // We write the field distribution into the main database
        self.index.put_field_distribution(self.wtxn, &field_distribution)?;

        // We write the primary key field id into the main database
        self.index.put_primary_key(self.wtxn, &primary_key)?;

        // We write the external documents ids into the main database.
        self.index.put_external_documents_ids(self.wtxn, &external_documents_ids)?;

        let all_documents_ids = index_documents_ids | new_documents_ids | replaced_documents_ids;
        self.index.put_documents_ids(self.wtxn, &all_documents_ids)?;

        self.execute_prefix_databases(
            word_docids,
            exact_word_docids,
            word_pair_proximity_docids,
            word_position_docids,
        )?;

        Ok(all_documents_ids.len())
    }

    #[logging_timer::time("IndexDocuments::{}")]
    pub fn execute_prefix_databases(
        self,
        word_docids: Option<grenad::Reader<CursorClonableMmap>>,
        exact_word_docids: Option<grenad::Reader<CursorClonableMmap>>,
        word_pair_proximity_docids: Option<grenad::Reader<CursorClonableMmap>>,
        word_position_docids: Option<grenad::Reader<CursorClonableMmap>>,
    ) -> Result<()>
    where
        F: Fn(UpdateIndexingStep) + Sync,
    {
        // Merged databases are already been indexed, we start from this count;
        let mut databases_seen = MERGED_DATABASE_COUNT;

        // Run the facets update operation.
        let mut builder = Facets::new(self.wtxn, self.index);
        builder.chunk_compression_type = self.indexer_config.chunk_compression_type;
        builder.chunk_compression_level = self.indexer_config.chunk_compression_level;
        if let Some(value) = self.config.facet_level_group_size {
            builder.level_group_size(value);
        }
        if let Some(value) = self.config.facet_min_level_size {
            builder.min_level_size(value);
        }
        builder.execute()?;

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        let previous_words_prefixes_fst =
            self.index.words_prefixes_fst(self.wtxn)?.map_data(|cow| cow.into_owned())?;

        // Run the words prefixes update operation.
        let mut builder = WordsPrefixesFst::new(self.wtxn, self.index);
        if let Some(value) = self.config.words_prefix_threshold {
            builder.threshold(value);
        }
        if let Some(value) = self.config.max_prefix_length {
            builder.max_prefix_length(value);
        }
        builder.execute()?;

        let current_prefix_fst = self.index.words_prefixes_fst(self.wtxn)?;

        // We retrieve the common words between the previous and new prefix word fst.
        let common_prefix_fst_words = fst_stream_into_vec(
            previous_words_prefixes_fst.op().add(&current_prefix_fst).intersection(),
        );
        let common_prefix_fst_words: Vec<_> = common_prefix_fst_words
            .as_slice()
            .linear_group_by_key(|x| x.chars().nth(0).unwrap())
            .collect();

        // We retrieve the newly added words between the previous and new prefix word fst.
        let new_prefix_fst_words = fst_stream_into_vec(
            current_prefix_fst.op().add(&previous_words_prefixes_fst).difference(),
        );

        // We compute the set of prefixes that are no more part of the prefix fst.
        let del_prefix_fst_words = fst_stream_into_hashset(
            previous_words_prefixes_fst.op().add(&current_prefix_fst).difference(),
        );

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        if let Some(word_docids) = word_docids {
            execute_word_prefix_docids(
                self.wtxn,
                word_docids,
                self.index.word_docids.clone(),
                self.index.word_prefix_docids.clone(),
                &self.indexer_config,
                &new_prefix_fst_words,
                &common_prefix_fst_words,
                &del_prefix_fst_words,
            )?;
        }

        if let Some(exact_word_docids) = exact_word_docids {
            execute_word_prefix_docids(
                self.wtxn,
                exact_word_docids,
                self.index.exact_word_docids.clone(),
                self.index.exact_word_prefix_docids.clone(),
                &self.indexer_config,
                &new_prefix_fst_words,
                &common_prefix_fst_words,
                &del_prefix_fst_words,
            )?;
        }

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        if let Some(word_pair_proximity_docids) = word_pair_proximity_docids {
            // Run the word prefix pair proximity docids update operation.
            let mut builder = WordPrefixPairProximityDocids::new(self.wtxn, self.index);
            builder.chunk_compression_type = self.indexer_config.chunk_compression_type;
            builder.chunk_compression_level = self.indexer_config.chunk_compression_level;
            builder.max_nb_chunks = self.indexer_config.max_nb_chunks;
            builder.max_memory = self.indexer_config.max_memory;
            builder.execute(
                word_pair_proximity_docids,
                &new_prefix_fst_words,
                &common_prefix_fst_words,
                &del_prefix_fst_words,
            )?;
        }

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        if let Some(word_position_docids) = word_position_docids {
            // Run the words prefix position docids update operation.
            let mut builder = WordPrefixPositionDocids::new(self.wtxn, self.index);
            builder.chunk_compression_type = self.indexer_config.chunk_compression_type;
            builder.chunk_compression_level = self.indexer_config.chunk_compression_level;
            builder.max_nb_chunks = self.indexer_config.max_nb_chunks;
            builder.max_memory = self.indexer_config.max_memory;
            if let Some(value) = self.config.words_positions_level_group_size {
                builder.level_group_size(value);
            }
            if let Some(value) = self.config.words_positions_min_level_size {
                builder.min_level_size(value);
            }
            builder.execute(
                word_position_docids,
                &new_prefix_fst_words,
                &common_prefix_fst_words,
                &del_prefix_fst_words,
            )?;
        }

        databases_seen += 1;
        (self.progress)(UpdateIndexingStep::MergeDataIntoFinalDatabase {
            databases_seen,
            total_databases: TOTAL_POSTING_DATABASE_COUNT,
        });

        Ok(())
    }
}

/// Run the word prefix docids update operation.
fn execute_word_prefix_docids(
    txn: &mut heed::RwTxn,
    reader: grenad::Reader<Cursor<ClonableMmap>>,
    word_docids_db: Database<Str, RoaringBitmapCodec>,
    word_prefix_docids_db: Database<Str, RoaringBitmapCodec>,
    indexer_config: &IndexerConfig,
    new_prefix_fst_words: &[String],
    common_prefix_fst_words: &[&[String]],
    del_prefix_fst_words: &HashSet<Vec<u8>>,
) -> Result<()> {
    let cursor = reader.into_cursor()?;
    let mut builder = WordPrefixDocids::new(txn, word_docids_db, word_prefix_docids_db);
    builder.chunk_compression_type = indexer_config.chunk_compression_type;
    builder.chunk_compression_level = indexer_config.chunk_compression_level;
    builder.max_nb_chunks = indexer_config.max_nb_chunks;
    builder.max_memory = indexer_config.max_memory;
    builder.execute(
        cursor,
        &new_prefix_fst_words,
        &common_prefix_fst_words,
        &del_prefix_fst_words,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use big_s::S;
    use heed::EnvOpenOptions;
    use maplit::hashset;

    use super::*;
    use crate::documents::DocumentBatchBuilder;
    use crate::update::DeleteDocuments;
    use crate::HashMap;

    #[test]
    fn simple_document_replacement() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with ids from 1 to 3.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 1, "name": "kevin" },
            { "id": 2, "name": "kevina" },
            { "id": 3, "name": "benoit" }
        ]);

        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 3 documents now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);
        drop(rtxn);

        // Second we send 1 document with id 1, to erase the previous ones.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([ { "id": 1, "name": "updated kevin" } ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is **always** 3 documents.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);
        drop(rtxn);

        // Third we send 3 documents again to replace the existing ones.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 1, "name": "updated second kevin" },
            { "id": 2, "name": "updated kevina" },
            { "id": 3, "name": "updated benoit" }
        ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        builder.add_documents(content).unwrap();
        wtxn.commit().unwrap();

        // Check that there is **always** 3 documents.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);
        drop(rtxn);
    }

    #[test]
    fn simple_document_merge() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with duplicate ids and
        // change the index method to merge documents.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 1, "name": "kevin" },
            { "id": 1, "name": "kevina" },
            { "id": 1, "name": "benoit" }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig {
            update_method: IndexDocumentsMethod::UpdateDocuments,
            ..Default::default()
        };
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is only 1 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 1);

        // Check that we get only one document from the database.
        let docs = index.documents(&rtxn, Some(0)).unwrap();
        assert_eq!(docs.len(), 1);
        let (id, doc) = docs[0];
        assert_eq!(id, 0);

        // Check that this document is equal to the last one sent.
        let mut doc_iter = doc.iter();
        assert_eq!(doc_iter.next(), Some((0, &b"1"[..])));
        assert_eq!(doc_iter.next(), Some((1, &br#""benoit""#[..])));
        assert_eq!(doc_iter.next(), None);
        drop(rtxn);

        // Second we send 1 document with id 1, to force it to be merged with the previous one.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([ { "id": 1, "age": 25 } ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is **always** 1 document.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 1);

        // Check that we get only one document from the database.
        let docs = index.documents(&rtxn, Some(0)).unwrap();
        assert_eq!(docs.len(), 1);
        let (id, doc) = docs[0];
        assert_eq!(id, 0);

        // Check that this document is equal to the last one sent.
        let mut doc_iter = doc.iter();
        assert_eq!(doc_iter.next(), Some((0, &b"1"[..])));
        assert_eq!(doc_iter.next(), Some((1, &br#""benoit""#[..])));
        assert_eq!(doc_iter.next(), Some((2, &b"25"[..])));
        assert_eq!(doc_iter.next(), None);
        drop(rtxn);
    }

    #[test]
    fn not_auto_generated_documents_ids() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with ids from 1 to 3.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "name": "kevin" },
            { "name": "kevina" },
            { "name": "benoit" }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        assert!(builder.add_documents(content).is_err());
        wtxn.commit().unwrap();

        // Check that there is no document.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 0);
        drop(rtxn);
    }

    #[test]
    fn simple_auto_generated_documents_ids() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with ids from 1 to 3.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "name": "kevin" },
            { "name": "kevina" },
            { "name": "benoit" }
        ]);
        let config = IndexerConfig::default();
        let indexing_config =
            IndexDocumentsConfig { autogenerate_docids: true, ..Default::default() };
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 3 documents now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);

        let docs = index.documents(&rtxn, vec![0, 1, 2]).unwrap();
        let (_id, obkv) = docs.iter().find(|(_id, kv)| kv.get(0) == Some(br#""kevin""#)).unwrap();
        let kevin_uuid: String = serde_json::from_slice(&obkv.get(1).unwrap()).unwrap();
        drop(rtxn);

        // Second we send 1 document with the generated uuid, to erase the previous ones.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([ { "name": "updated kevin", "id": kevin_uuid } ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is **always** 3 documents.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);

        let docs = index.documents(&rtxn, vec![0, 1, 2]).unwrap();
        let (kevin_id, _) =
            docs.iter().find(|(_, d)| d.get(0).unwrap() == br#""updated kevin""#).unwrap();
        let (id, doc) = docs[*kevin_id as usize];
        assert_eq!(id, *kevin_id);

        // Check that this document is equal to the last
        // one sent and that an UUID has been generated.
        assert_eq!(doc.get(0), Some(&br#""updated kevin""#[..]));
        // This is an UUID, it must be 36 bytes long plus the 2 surrounding string quotes (").
        assert_eq!(doc.get(1).unwrap().len(), 36 + 2);
        drop(rtxn);
    }

    #[test]
    fn reordered_auto_generated_documents_ids() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with ids from 1 to 3.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 1, "name": "kevin" },
            { "id": 2, "name": "kevina" },
            { "id": 3, "name": "benoit" }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 3 documents now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 3);
        drop(rtxn);

        // Second we send 1 document without specifying the id.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([ { "name": "new kevin" } ]);
        let indexing_config =
            IndexDocumentsConfig { autogenerate_docids: true, ..Default::default() };
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 4 documents now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 4);
        drop(rtxn);
    }

    #[test]
    fn empty_update() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 0 documents and only headers.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is no documents.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 0);
        drop(rtxn);
    }

    #[test]
    fn invalid_documents_ids() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 1 document with an invalid id.
        let mut wtxn = index.write_txn().unwrap();
        // There is a space in the document id.
        let content = documents!([ { "id": "brume bleue", "name": "kevin" } ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        assert!(builder.add_documents(content).is_err());
        wtxn.commit().unwrap();

        // First we send 1 document with a valid id.
        let mut wtxn = index.write_txn().unwrap();
        // There is a space in the document id.
        let content = documents!([ { "id": 32, "name": "kevin" } ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 1 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 1);
        drop(rtxn);
    }

    #[test]
    fn complex_documents() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with an id for only one of them.
        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "id": 0, "name": "kevin", "object": { "key1": "value1", "key2": "value2" } },
            { "id": 1, "name": "kevina", "array": ["I", "am", "fine"] },
            { "id": 2, "name": "benoit", "array_of_object": [{ "wow": "amazing" }] }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 1 documents now.
        let rtxn = index.read_txn().unwrap();

        // Search for a sub object value
        let result = index.search(&rtxn).query(r#""value2""#).execute().unwrap();
        assert_eq!(result.documents_ids, vec![0]);

        // Search for a sub array value
        let result = index.search(&rtxn).query(r#""fine""#).execute().unwrap();
        assert_eq!(result.documents_ids, vec![1]);

        // Search for a sub array sub object key
        let result = index.search(&rtxn).query(r#""amazing""#).execute().unwrap();
        assert_eq!(result.documents_ids, vec![2]);

        drop(rtxn);
    }

    #[test]
    fn simple_documents_replace() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // First we send 3 documents with an id for only one of them.
        let mut wtxn = index.write_txn().unwrap();
        let documents = documents!([
          { "id": 2,    "title": "Pride and Prejudice",                    "author": "Jane Austin",              "genre": "romance",    "price": 3.5, "_geo": { "lat": 12, "lng": 42 } },
          { "id": 456,  "title": "Le Petit Prince",                        "author": "Antoine de Saint-Exupéry", "genre": "adventure" , "price": 10.0 },
          { "id": 1,    "title": "Alice In Wonderland",                    "author": "Lewis Carroll",            "genre": "fantasy",    "price": 25.99 },
          { "id": 1344, "title": "The Hobbit",                             "author": "J. R. R. Tolkien",         "genre": "fantasy" },
          { "id": 4,    "title": "Harry Potter and the Half-Blood Prince", "author": "J. K. Rowling",            "genre": "fantasy" },
          { "id": 42,   "title": "The Hitchhiker's Guide to the Galaxy",   "author": "Douglas Adams", "_geo": { "lat": 35, "lng": 23 } }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig {
            update_method: IndexDocumentsMethod::ReplaceDocuments,
            ..Default::default()
        };
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        builder.add_documents(documents).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let indexing_config = IndexDocumentsConfig {
            update_method: IndexDocumentsMethod::UpdateDocuments,
            ..Default::default()
        };
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config, |_| ()).unwrap();
        let documents = documents!([
          {
            "id": 2,
            "author": "J. Austen",
            "date": "1813"
          }
        ]);

        builder.add_documents(documents).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn index_all_flavour_of_geo() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let config = IndexerConfig::default();
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = update::Settings::new(&mut wtxn, &index, &config);

        builder.set_filterable_fields(hashset!(S("_geo")));
        builder.execute(|_| ()).unwrap();
        wtxn.commit().unwrap();

        let indexing_config = IndexDocumentsConfig {
            update_method: IndexDocumentsMethod::ReplaceDocuments,
            ..Default::default()
        };
        let mut wtxn = index.write_txn().unwrap();

        let documents = documents!([
          { "id": 0, "_geo": { "lat": 31, "lng": [42] } },
          { "id": 1, "_geo": { "lat": "31" }, "_geo.lng": 42 },
          { "id": 2, "_geo": { "lng": "42" }, "_geo.lat": "31" },
          { "id": 3, "_geo.lat": 31, "_geo.lng": "42" },
        ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(documents).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        let mut search = crate::Search::new(&rtxn, &index);
        search.filter(crate::Filter::from_str("_geoRadius(31, 42, 0.000001)").unwrap().unwrap());
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![0, 1, 2, 3]);
    }

    #[test]
    fn geo_error() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let config = IndexerConfig::default();
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = update::Settings::new(&mut wtxn, &index, &config);

        builder.set_filterable_fields(hashset!(S("_geo")));
        builder.execute(|_| ()).unwrap();
        wtxn.commit().unwrap();

        let indexing_config = IndexDocumentsConfig {
            update_method: IndexDocumentsMethod::ReplaceDocuments,
            ..Default::default()
        };
        let mut wtxn = index.write_txn().unwrap();

        let documents = documents!([
          { "id": 0, "_geo": { "lng": 42 } }
        ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(documents).unwrap();
        let error = builder.execute().unwrap_err();
        assert_eq!(
            &error.to_string(),
            r#"Could not find latitude in the document with the id: `0`. Was expecting a `_geo.lat` field."#
        );

        let documents = documents!([
          { "id": 0, "_geo": { "lat": 42 } }
        ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(documents).unwrap();
        let error = builder.execute().unwrap_err();
        assert_eq!(
            &error.to_string(),
            r#"Could not find longitude in the document with the id: `0`. Was expecting a `_geo.lng` field."#
        );

        let documents = documents!([
          { "id": 0, "_geo": { "lat": "lol", "lng": 42 } }
        ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(documents).unwrap();
        let error = builder.execute().unwrap_err();
        assert_eq!(
            &error.to_string(),
            r#"Could not parse latitude in the document with the id: `0`. Was expecting a number but instead got `"lol"`."#
        );

        let documents = documents!([
          { "id": 0, "_geo": { "lat": [12, 13], "lng": 42 } }
        ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(documents).unwrap();
        let error = builder.execute().unwrap_err();
        assert_eq!(
            &error.to_string(),
            r#"Could not parse latitude in the document with the id: `0`. Was expecting a number but instead got `[12,13]`."#
        );

        let documents = documents!([
          { "id": 0, "_geo": { "lat": 12, "lng": "hello" } }
        ]);
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(documents).unwrap();
        let error = builder.execute().unwrap_err();
        assert_eq!(
            &error.to_string(),
            r#"Could not parse longitude in the document with the id: `0`. Was expecting a number but instead got `"hello"`."#
        );
    }

    #[test]
    fn delete_documents_then_insert() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            { "objectId": 123, "title": "Pride and Prejudice", "comment": "A great book" },
            { "objectId": 456, "title": "Le Petit Prince",     "comment": "A french book" },
            { "objectId": 1,   "title": "Alice In Wonderland", "comment": "A weird book" },
            { "objectId": 30,  "title": "Hamlet", "_geo": { "lat": 12, "lng": 89 } }
        ]);
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        assert_eq!(index.primary_key(&wtxn).unwrap(), Some("objectId"));

        // Delete not all of the documents but some of them.
        let mut builder = DeleteDocuments::new(&mut wtxn, &index).unwrap();
        builder.delete_external_id("30");
        builder.execute().unwrap();

        let external_documents_ids = index.external_documents_ids(&wtxn).unwrap();
        assert!(external_documents_ids.get("30").is_none());

        let content = documents!([
            { "objectId": 30,  "title": "Hamlet", "_geo": { "lat": 12, "lng": 89 } }
        ]);

        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        let external_documents_ids = index.external_documents_ids(&wtxn).unwrap();
        assert!(external_documents_ids.get("30").is_some());

        let content = documents!([
            { "objectId": 30,  "title": "Hamlet", "_geo": { "lat": 12, "lng": 89 } }
        ]);

        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();
    }

    #[test]
    fn index_more_than_256_fields() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();

        let mut big_object = HashMap::new();
        big_object.insert(S("id"), "wow");
        for i in 0..1000 {
            let key = i.to_string();
            big_object.insert(key, "I am a text!");
        }

        let mut cursor = Cursor::new(Vec::new());

        let mut builder = DocumentBatchBuilder::new(&mut cursor).unwrap();
        let big_object = Cursor::new(serde_json::to_vec(&big_object).unwrap());
        builder.extend_from_json(big_object).unwrap();
        builder.finish().unwrap();
        cursor.set_position(0);
        let content = DocumentBatchReader::from_reader(cursor).unwrap();

        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();
    }

    #[test]
    fn index_more_than_1000_positions_in_a_field() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(50 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();

        let mut big_object = HashMap::new();
        big_object.insert(S("id"), "wow");
        let content: String = (0..=u16::MAX)
            .into_iter()
            .map(|p| p.to_string())
            .reduce(|a, b| a + " " + b.as_ref())
            .unwrap();
        big_object.insert("content".to_string(), &content);

        let mut cursor = Cursor::new(Vec::new());

        let big_object = serde_json::to_string(&big_object).unwrap();
        let mut builder = DocumentBatchBuilder::new(&mut cursor).unwrap();
        builder.extend_from_json(&mut big_object.as_bytes()).unwrap();
        builder.finish().unwrap();
        cursor.set_position(0);
        let content = DocumentBatchReader::from_reader(cursor).unwrap();

        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();

        let mut rtxn = index.read_txn().unwrap();

        assert!(index.word_docids.get(&mut rtxn, "0").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "64").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "256").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "1024").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "32768").unwrap().is_some());
        assert!(index.word_docids.get(&mut rtxn, "65535").unwrap().is_some());
    }

    #[test]
    fn index_documents_with_zeroes() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            {
                "id": 2,
                "title": "Prideand Prejudice",
                "au{hor": "Jane Austin",
                "genre": "romance",
                "price$": "3.5$",
            },
            {
                "id": 456,
                "title": "Le Petit Prince",
                "au{hor": "Antoine de Saint-Exupéry",
                "genre": "adventure",
                "price$": "10.0$",
            },
            {
                "id": 1,
                "title": "Wonderland",
                "au{hor": "Lewis Carroll",
                "genre": "fantasy",
                "price$": "25.99$",
            },
            {
                "id": 4,
                "title": "Harry Potter ing fantasy\0lood Prince",
                "au{hor": "J. K. Rowling",
                "genre": "fantasy\0",
            },
        ]);

        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();
    }

    #[test]
    fn index_documents_with_nested_fields() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            {
                "id": 0,
                "title": "The zeroth document",
            },
            {
                "id": 1,
                "title": "The first document",
                "nested": {
                    "object": "field",
                    "machin": "bidule",
                },
            },
            {
                "id": 2,
                "title": "The second document",
                "nested": [
                    "array",
                    {
                        "object": "field",
                    },
                    {
                        "prout": "truc",
                        "machin": "lol",
                    },
                ],
            },
            {
                "id": 3,
                "title": "The third document",
                "nested": "I lied",
            },
        ]);

        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let mut builder = update::Settings::new(&mut wtxn, &index, &config);

        let searchable_fields = vec![S("title"), S("nested.object"), S("nested.machin")];
        builder.set_searchable_fields(searchable_fields);

        let faceted_fields = hashset!(S("title"), S("nested.object"), S("nested.machin"));
        builder.set_filterable_fields(faceted_fields);
        builder.execute(|_| ()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        let facets = index.faceted_fields(&rtxn).unwrap();
        assert_eq!(facets, hashset!(S("title"), S("nested.object"), S("nested.machin")));

        // testing the simple query search
        let mut search = crate::Search::new(&rtxn, &index);
        search.query("document");
        search.authorize_typos(true);
        search.optional_words(true);
        // all documents should be returned
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids.len(), 4);

        search.query("zeroth");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![0]);
        search.query("first");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![1]);
        search.query("second");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![2]);
        search.query("third");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![3]);

        search.query("field");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![1, 2]);

        search.query("lol");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![2]);

        search.query("object");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert!(documents_ids.is_empty());

        search.query("array");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert!(documents_ids.is_empty()); // nested is not searchable

        search.query("lied");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert!(documents_ids.is_empty()); // nested is not searchable

        // testing the filters
        let mut search = crate::Search::new(&rtxn, &index);
        search.filter(crate::Filter::from_str(r#"title = "The first document""#).unwrap().unwrap());
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![1]);

        search.filter(crate::Filter::from_str(r#"nested.object = field"#).unwrap().unwrap());
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![1, 2]);

        search.filter(crate::Filter::from_str(r#"nested.machin = bidule"#).unwrap().unwrap());
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![1]);

        search.filter(crate::Filter::from_str(r#"nested = array"#).unwrap().unwrap());
        let error = search.execute().map(|_| unreachable!()).unwrap_err(); // nested is not filterable
        assert!(matches!(error, crate::Error::UserError(crate::UserError::InvalidFilter(_))));

        search.filter(crate::Filter::from_str(r#"nested = "I lied""#).unwrap().unwrap());
        let error = search.execute().map(|_| unreachable!()).unwrap_err(); // nested is not filterable
        assert!(matches!(error, crate::Error::UserError(crate::UserError::InvalidFilter(_))));
    }

    #[test]
    fn index_documents_with_nested_primary_key() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();
        let config = IndexerConfig::default();

        let mut wtxn = index.write_txn().unwrap();
        let mut builder = update::Settings::new(&mut wtxn, &index, &config);
        builder.set_primary_key("complex.nested.id".to_owned());
        builder.execute(|_| ()).unwrap();
        wtxn.commit().unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            {
                "complex": {
                    "nested": {
                        "id": 0,
                    },
                },
                "title": "The zeroth document",
            },
            {
                "complex.nested": {
                    "id": 1,
                },
                "title": "The first document",
            },
            {
                "complex": {
                    "nested.id": 2,
                },
                "title": "The second document",
            },
            {
                "complex.nested.id": 3,
                "title": "The third document",
            },
        ]);

        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        // testing the simple query search
        let mut search = crate::Search::new(&rtxn, &index);
        search.query("document");
        search.authorize_typos(true);
        search.optional_words(true);
        // all documents should be returned
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids.len(), 4);

        search.query("zeroth");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![0]);
        search.query("first");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![1]);
        search.query("second");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![2]);
        search.query("third");
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![3]);
    }

    #[test]
    fn test_facets_generation() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let content = documents!([
            {
                "id": 0,
                "dog": {
                    "race": {
                        "bernese mountain": "zeroth",
                    },
                },
            },
            {
                "id": 1,
                "dog.race": {
                    "bernese mountain": "first",
                },
            },
            {
                "id": 2,
                "dog.race.bernese mountain": "second",
            },
            {
                "id": 3,
                "dog": {
                    "race.bernese mountain": "third"
                },
            },
        ]);

        // index the documents
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();

        // ---- ADD THE SETTING TO TEST THE FILTERABLE

        // add the settings
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = update::Settings::new(&mut wtxn, &index, &config);

        builder.set_filterable_fields(hashset!(String::from("dog")));

        builder.execute(|_| ()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        let hidden = index.faceted_fields(&rtxn).unwrap();

        assert_eq!(hidden, hashset!(S("dog"), S("dog.race"), S("dog.race.bernese mountain")));

        for (s, i) in [("zeroth", 0), ("first", 1), ("second", 2), ("third", 3)] {
            let mut search = crate::Search::new(&rtxn, &index);
            let filter = format!(r#""dog.race.bernese mountain" = {s}"#);
            search.filter(crate::Filter::from_str(&filter).unwrap().unwrap());
            let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
            assert_eq!(documents_ids, vec![i]);
        }

        // ---- RESET THE SETTINGS

        // update the settings
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = update::Settings::new(&mut wtxn, &index, &config);

        builder.reset_filterable_fields();

        builder.execute(|_| ()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        let facets = index.faceted_fields(&rtxn).unwrap();

        assert_eq!(facets, hashset!());

        // ---- UPDATE THE SETTINGS TO TEST THE SORTABLE

        // update the settings
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = update::Settings::new(&mut wtxn, &index, &config);

        builder.set_sortable_fields(hashset!(S("dog.race")));

        builder.execute(|_| ()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        let facets = index.faceted_fields(&rtxn).unwrap();

        assert_eq!(facets, hashset!(S("dog.race"), S("dog.race.bernese mountain")));

        let mut search = crate::Search::new(&rtxn, &index);
        search.sort_criteria(vec![crate::AscDesc::Asc(crate::Member::Field(S(
            "dog.race.bernese mountain",
        )))]);
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![1, 2, 3, 0]);
    }

    #[test]
    fn index_2_times_documents_split_by_zero_document_indexation() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let content = documents!([
            {"id": 0, "name": "Kerollmops", "score": 78},
            {"id": 1, "name": "ManyTheFish", "score": 75},
            {"id": 2, "name": "Ferdi", "score": 39},
            {"id": 3, "name": "Tommy", "score": 33}
        ]);

        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 4 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 4);

        let content = documents!([]);

        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 4 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 4);

        let content = documents!([
            {"id": 0, "name": "Kerollmops", "score": 78},
            {"id": 1, "name": "ManyTheFish", "score": 75},
            {"id": 2, "name": "Ferdi", "score": 39},
            {"id": 3, "name": "Tommy", "score": 33}
        ]);

        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        // Check that there is 4 document now.
        let rtxn = index.read_txn().unwrap();
        let count = index.number_of_documents(&rtxn).unwrap();
        assert_eq!(count, 4);
    }

    #[test]
    fn test_meilisearch_1714() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let content = documents!([
          {"id": "123", "title": "小化妆包" },
          {"id": "456", "title": "Ipad 包" }
        ]);

        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        // Only the first document should match.
        let count = index.word_docids.get(&rtxn, "化妆包").unwrap().unwrap().len();
        assert_eq!(count, 1);

        // Only the second document should match.
        let count = index.word_docids.get(&rtxn, "包").unwrap().unwrap().len();
        assert_eq!(count, 1);

        let mut search = crate::Search::new(&rtxn, &index);
        search.query("化妆包");
        search.authorize_typos(true);
        search.optional_words(true);

        // only 1 document should be returned
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids.len(), 1);
    }

    /// We try to index documents with words that are too long here,
    /// it should not return any error.
    #[test]
    fn text_with_too_long_words() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let content = documents!([
          {"id": 1, "title": "a".repeat(256) },
          {"id": 2, "title": "b".repeat(512) },
          {"id": 3, "title": format!("{} {}", "c".repeat(250), "d".repeat(250)) },
        ]);

        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn text_with_too_long_keys() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();
        let script = "https://bug.example.com/meilisearch/milli.saml2?ROLE=Programmer-1337&SAMLRequest=Cy1ytcZT1Po%2L2IY2y9Unru8rgnW4qWfPiI0EpT7P8xjJV8PeQikRL%2E8D9A4pj9tmbymbQCQwGmGjPMK7qwXFPX4DH52JO2b7n6TXjuR7zkIFuYdzdY2rwRNBPgCL7ihclEm9zyIjKZQ%2JTqiwfXxWjnI0KEYQYHdwd6Q%2Fx%28BDLNsvmL54CCY2F4RWeRs4eqWfn%2EHqxlhreFzax4AiQ2tgOtV5thOaaWqrhZD%2Py70nuyZWNTKwciGI43AoHg6PThANsQ5rAY5amzN%2ufbs1swETUXlLZuOut5YGpYPZfY6STJWNp4QYSUOUXBZpdElYsH7UHZ7VhJycgyt%28aTK0GW6GbKne2tJM0hgSczOqndg6RFa9WsnSBi4zMcaEfYur4WlSsHDYInF9ROousKqVMZ6H8%2gbUissaLh1eXRGo8KEJbyEHbhVVKGD%28kx4cfKjx9fT3pkeDTdvDrVn25jIzi9wHyt9l1lWc8ICnCvXCVUPP%2BjBG4wILR29gMV9Ux2QOieQm2%2Fycybhr8sBGCl30mHC7blvWt%2T3mrCHQoS3VK49PZNPqBZO9C7vOjOWoszNkJx4QckWV%2FZFvbpzUUkiBiehr9F%2FvQSxz9lzv68GwbTu9fr638p%2FQM%3D&RelayState=https%3A%2F%example.bug.com%2Fde&SigAlg=http%3A%2F%2Fwww.w3.org%2F2000%2F09%2Fxmldsig%23rsa-sha1&Signature=AZFpkhFFII7PodiewTovaGnLQKUVZp0qOCCcBIUkJ6P5by3lE3Lldj9pKaFu4wz4j%2B015HEhDvF0LlAmwwES85vdGh%2FpD%2cIQPRUEjdCbQkQDd3dy1mMXbpXxSe4QYcv9Ni7tqNTQxekpO1gE7rtg6zC66EU55uM9aj9abGQ034Vly%2F6IJ08bvAq%2B%2FB9KruLstuiNWnlXTfNGsOxGLK7%2BXr94LTkat8m%2FMan6Qr95%2KeR5TmmqaQIE4N9H6o4TopT7mXr5CF2Z3";

        // Create 200 documents with a long text
        let content = {
            let documents: Vec<_> = (0..200i32)
                .into_iter()
                .map(|i| serde_json::json!({ "id": i, "script": script }))
                .collect();

            let mut writer = std::io::Cursor::new(Vec::new());
            let mut builder = crate::documents::DocumentBatchBuilder::new(&mut writer).unwrap();
            let documents = serde_json::to_vec(&documents).unwrap();
            builder.extend_from_json(std::io::Cursor::new(documents)).unwrap();
            builder.finish().unwrap();
            writer.set_position(0);
            crate::documents::DocumentBatchReader::from_reader(writer).unwrap()
        };

        // Index those 200 long documents
        let mut wtxn = index.write_txn().unwrap();
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        // Create one long document
        let content = documents!([
          {"id": 400, "script": script },
        ]);

        // Index this one long document
        let config = IndexerConfig::default();
        let indexing_config = IndexDocumentsConfig::default();
        let mut builder =
            IndexDocuments::new(&mut wtxn, &index, &config, indexing_config.clone(), |_| ())
                .unwrap();
        builder.add_documents(content).unwrap();
        builder.execute().unwrap();

        wtxn.commit().unwrap();
    }
}
