use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

use common::cpu::CpuPermit;
use segment::data_types::index::{IntegerIndexParams, KeywordIndexParams};
use segment::data_types::vectors::{only_default_vector, DEFAULT_VECTOR_NAME};
use segment::entry::entry_point::SegmentEntry;
use segment::index::hnsw_index::num_rayon_threads;
use segment::json_path::JsonPath;
use segment::segment::Segment;
use segment::segment_constructor::segment_builder::SegmentBuilder;
use segment::segment_constructor::{build_segment, load_segment};
use segment::types::{
    Distance, HnswConfig, Indexes, PayloadFieldSchema, PayloadSchemaParams, PayloadStorageType,
    SegmentConfig, VectorDataConfig, VectorStorageType,
};
use tempfile::Builder;

/// This test tests snapshotting and restoring a segment with all on-disk components.
#[test]
fn test_on_disk_segment_snapshot() {
    let data = r#"
    {
        "names": ["John Doe", "Bill Murray"],
        "ages": [43, 51],
        "metadata": {
            "height": 50,
            "width": 60
        }
    }"#;

    let segment_builder_dir = Builder::new().prefix("segment_dir").tempdir().unwrap();
    let building_config = SegmentConfig {
        vector_data: HashMap::from([(
            DEFAULT_VECTOR_NAME.to_owned(),
            VectorDataConfig {
                size: 2,
                distance: Distance::Dot,
                storage_type: VectorStorageType::Memory,
                index: Indexes::Plain {},
                quantization_config: None,
                multivector_config: None,
                datatype: None,
            },
        )]),
        sparse_vector_data: Default::default(),
        payload_storage_type: Default::default(),
    };

    let mut segment = build_segment(segment_builder_dir.path(), &building_config, true).unwrap();

    segment
        .upsert_point(0, 0.into(), only_default_vector(&[1.0, 1.0]))
        .unwrap();
    segment
        .upsert_point(1, 1.into(), only_default_vector(&[2.0, 2.0]))
        .unwrap();

    segment
        .set_full_payload(2, 0.into(), &serde_json::from_str(data).unwrap())
        .unwrap();
    segment
        .set_full_payload(3, 0.into(), &serde_json::from_str(data).unwrap())
        .unwrap();

    segment
        .create_field_index(
            4,
            &JsonPath::new("names"),
            Some(&PayloadFieldSchema::FieldParams(
                PayloadSchemaParams::Keyword(KeywordIndexParams {
                    r#type: segment::data_types::index::KeywordIndexType::Keyword,
                    is_tenant: None,
                    on_disk: Some(true),
                }),
            )),
        )
        .unwrap();
    segment
        .create_field_index(
            5,
            &JsonPath::new("ages"),
            Some(&PayloadFieldSchema::FieldParams(
                PayloadSchemaParams::Integer(IntegerIndexParams {
                    r#type: segment::data_types::index::IntegerIndexType::Integer,
                    lookup: true,
                    range: true,
                    is_principal: None,
                    on_disk: Some(true),
                }),
            )),
        )
        .unwrap();

    let segment_config = SegmentConfig {
        vector_data: HashMap::from([(
            DEFAULT_VECTOR_NAME.to_owned(),
            VectorDataConfig {
                size: 2,
                distance: Distance::Dot,
                storage_type: VectorStorageType::Mmap, // mmap vectors
                index: Indexes::Hnsw(HnswConfig {
                    m: 4,
                    ef_construct: 16,
                    full_scan_threshold: 8,
                    max_indexing_threads: 2,
                    on_disk: Some(true), // mmap index
                    payload_m: None,
                }),
                quantization_config: None,
                multivector_config: None,
                datatype: None,
            },
        )]),
        sparse_vector_data: Default::default(),
        payload_storage_type: PayloadStorageType::OnDisk, // on-disk payload
    };

    let segment_base_dir = Builder::new().prefix("segment_dir").tempdir().unwrap();
    let segment_builder_dir = Builder::new().prefix("segment_dir").tempdir().unwrap();
    let mut segment_builder = SegmentBuilder::new(
        segment_base_dir.path(),
        segment_builder_dir.path(),
        &segment_config,
    )
    .unwrap();
    segment_builder.update(&[&segment], &false.into()).unwrap();
    let segment = segment_builder
        .build(CpuPermit::dummy(num_rayon_threads(0) as u32), &false.into())
        .unwrap();

    let snapshot_dir = Builder::new().prefix("snapshot_dir").tempdir().unwrap();
    let temp_dir = Builder::new().prefix("temp_dir").tempdir().unwrap();

    // take snapshot
    let archive = segment
        .take_snapshot(temp_dir.path(), snapshot_dir.path())
        .unwrap();
    let archive_extension = archive.extension().unwrap();
    let archive_name = archive.file_name().unwrap().to_str().unwrap().to_string();

    // correct file extension
    assert_eq!(archive_extension, "tar");

    // archive name contains segment id
    let segment_id = segment
        .current_path
        .file_stem()
        .and_then(|f| f.to_str())
        .unwrap();
    assert!(archive_name.starts_with(segment_id));

    // restore snapshot
    Segment::restore_snapshot(&archive, segment_id).unwrap();

    let restored_segment = load_segment(
        &snapshot_dir.path().join(segment_id),
        &AtomicBool::new(false),
    )
    .unwrap()
    .unwrap();

    // validate restored snapshot is the same as original segment
    assert_eq!(
        segment.total_point_count(),
        restored_segment.total_point_count(),
    );
    assert_eq!(
        segment.available_point_count(),
        restored_segment.available_point_count(),
    );
    assert_eq!(
        segment.deleted_point_count(),
        restored_segment.deleted_point_count(),
    );

    for id in segment.iter_points() {
        let vectors = segment.all_vectors(id).unwrap();
        let restored_vectors = restored_segment.all_vectors(id).unwrap();
        assert_eq!(vectors, restored_vectors);

        let payload = segment.payload(id).unwrap();
        let restored_payload = restored_segment.payload(id).unwrap();
        assert_eq!(payload, restored_payload);
    }
}
