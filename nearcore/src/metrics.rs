use near_o11y::metrics::{
    exponential_buckets, linear_buckets, try_create_histogram_vec, try_create_int_counter_vec,
    try_create_int_gauge, try_create_int_gauge_vec, HistogramVec, IntCounterVec, IntGauge,
    IntGaugeVec,
};
use near_store::{NodeStorage, Store, Temperature};
use once_cell::sync::Lazy;

pub static APPLY_CHUNK_DELAY: Lazy<HistogramVec> = Lazy::new(|| {
    try_create_histogram_vec(
        "near_apply_chunk_delay_seconds",
        "Time to process a chunk. Gas used by the chunk is a metric label, rounded up to 100 teragas.",
        &["tgas_ceiling"],
        Some(linear_buckets(0.0, 0.05, 50).unwrap()),
    )
        .unwrap()
});

pub(crate) static CONFIG_CORRECT: Lazy<IntGauge> = Lazy::new(|| {
    try_create_int_gauge(
        "near_config_correct",
        "Are the current dynamically loadable configs correct",
    )
    .unwrap()
});

pub(crate) static COLD_STORE_COPY_RESULT: Lazy<IntCounterVec> = Lazy::new(|| {
    try_create_int_counter_vec(
        "near_cold_store_copy_result",
        "The result of a cold store copy iteration in the cold store loop.",
        &["copy_result"],
    )
    .unwrap()
});

pub(crate) static STATE_SYNC_DUMP_ITERATION_ELAPSED: Lazy<HistogramVec> = Lazy::new(|| {
    try_create_histogram_vec(
        "near_state_sync_dump_iteration_elapsed_sec",
        "Time needed to obtain and write a part",
        &["shard_id"],
        Some(exponential_buckets(0.001, 1.6, 25).unwrap()),
    )
    .unwrap()
});
pub(crate) static STATE_SYNC_DUMP_PUT_OBJECT_ELAPSED: Lazy<HistogramVec> = Lazy::new(|| {
    try_create_histogram_vec(
        "near_state_sync_dump_put_object_elapsed_sec",
        "Time needed to write a part",
        &["shard_id"],
        Some(exponential_buckets(0.001, 1.6, 25).unwrap()),
    )
    .unwrap()
});
pub(crate) static STATE_SYNC_DUMP_NUM_PARTS_TOTAL: Lazy<IntGaugeVec> = Lazy::new(|| {
    try_create_int_gauge_vec(
        "near_state_sync_dump_num_parts_total",
        "Total number of parts in the epoch that being dumped",
        &["shard_id"],
    )
    .unwrap()
});
pub(crate) static STATE_SYNC_DUMP_NUM_PARTS_DUMPED: Lazy<IntGaugeVec> = Lazy::new(|| {
    try_create_int_gauge_vec(
        "near_state_sync_dump_num_parts_dumped",
        "Number of parts dumped in the epoch that is being dumped",
        &["shard_id"],
    )
    .unwrap()
});
pub(crate) static STATE_SYNC_DUMP_SIZE_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    try_create_int_counter_vec(
        "near_state_sync_dump_size_total",
        "Total size of parts written to S3",
        &["shard_id"],
    )
    .unwrap()
});
pub(crate) static STATE_SYNC_DUMP_EPOCH_HEIGHT: Lazy<IntGaugeVec> = Lazy::new(|| {
    try_create_int_gauge_vec(
        "near_state_sync_dump_epoch_height",
        "Epoch Height of an epoch being dumped",
        &["shard_id"],
    )
    .unwrap()
});
pub static STATE_SYNC_APPLY_PART_DELAY: Lazy<HistogramVec> = Lazy::new(|| {
    try_create_histogram_vec(
        "near_state_sync_apply_part_delay_sec",
        "Time needed to apply a state part",
        &["shard_id"],
        Some(exponential_buckets(0.001, 2.0, 20).unwrap()),
    )
    .unwrap()
});
pub static STATE_SYNC_OBTAIN_PART_DELAY: Lazy<HistogramVec> = Lazy::new(|| {
    try_create_histogram_vec(
        "near_state_sync_obtain_part_delay_sec",
        "Time needed to obtain a part",
        &["shard_id"],
        Some(exponential_buckets(0.001, 2.0, 20).unwrap()),
    )
    .unwrap()
});

fn export_store_stats(store: &Store, temperature: Temperature) {
    if let Some(stats) = store.get_store_statistics() {
        tracing::debug!(target:"metrics", "Exporting the db metrics for {temperature:?} store.");
        near_client::export_stats_as_metrics(stats, temperature);
    } else {
        // TODO Does that happen under normal circumstances?
        // Should this log be a warning?
        tracing::debug!(target:"metrics", "Exporting the db metrics for {temperature:?} store failed. The statistics are missing.");
    }
}

pub fn spawn_db_metrics_loop(
    storage: &NodeStorage,
    period: std::time::Duration,
) -> anyhow::Result<actix_rt::ArbiterHandle> {
    tracing::debug!(target:"metrics", "Spawning the db metrics loop.");
    let db_metrics_arbiter = actix_rt::Arbiter::new();
    let db_metrics_arbiter_handle = db_metrics_arbiter.handle();

    let start = tokio::time::Instant::now();
    let mut interval = actix_rt::time::interval_at(start, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let hot_store = storage.get_hot_store();
    let cold_store = storage.get_cold_store();

    db_metrics_arbiter.spawn(async move {
        tracing::debug!(target:"metrics", "Starting the db metrics loop.");
        loop {
            interval.tick().await;

            export_store_stats(&hot_store, Temperature::Hot);
            if let Some(cold_store) = &cold_store {
                export_store_stats(cold_store, Temperature::Cold);
            }
        }
    });
    Ok(db_metrics_arbiter_handle)
}
