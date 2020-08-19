use std::{
    collections::{BTreeMap, HashMap},
    mem, thread,
    time::{self, Duration, SystemTime},
};

use {
    futures::{channel::mpsc, future, prelude::*, stream},
    metrics::{Key, Recorder},
    rusoto_cloudwatch::{CloudWatch, Dimension, MetricDatum, PutMetricDataInput, StatisticSet},
    rusoto_core::Region,
};

use crate::{error::Error, BoxFuture};

pub type ClientBuilder = Box<dyn Fn(Region) -> Box<dyn CloudWatch + Send + Sync> + Send + Sync>;
type Count = usize;
type HistogramValue = u64;
type Timestamp = u64;

const MAX_CW_METRICS_PER_CALL: usize = 20;
const MAX_CLOUDWATCH_DIMENSIONS: usize = 10;
const MAX_HISTOGRAM_VALUES: usize = 150;
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Config {
    pub cloudwatch_namespace: String,
    pub default_dimensions: BTreeMap<String, String>,
    pub storage_resolution: Resolution,
    pub send_interval_secs: u64,
    pub client: Box<dyn CloudWatch + Send + Sync>,
    pub shutdown_signal: future::Shared<BoxFuture<'static, ()>>,
}

struct CollectorConfig {
    default_dimensions: BTreeMap<String, String>,
    storage_resolution: Resolution,
}

#[derive(Clone, Copy, Debug)]
pub enum Resolution {
    Second,
    Minute,
}

enum Message {
    Datum(Datum),
    SendBatch {
        send_all_before: Timestamp,
        emit_sender: mpsc::Sender<Vec<MetricDatum>>,
    },
}

#[derive(Debug)]
enum Value {
    Counter(u64),
    Gauge(i64),
    Histogram(u64),
}

#[derive(Clone, Debug, Default)]
pub struct Counter {
    sample_count: u64,
    sum: u64,
}

#[derive(Clone, Debug, Default)]
struct Aggregate {
    counter: Counter,
    gauge: StatisticSet,
    histogram: HashMap<HistogramValue, Count>,
}

struct Collector {
    metrics_data: BTreeMap<Timestamp, HashMap<Key, Aggregate>>,
    config: CollectorConfig,
}

#[derive(Debug)]
struct Datum {
    key: Key,
    value: Value,
}

#[derive(Debug, Default)]
struct Histogram {
    counts: Vec<f64>,
    values: Vec<f64>,
}

struct HistogramDatum {
    count: f64,
    value: f64,
}

pub struct RecorderHandle(mpsc::Sender<Datum>);

pub fn init(config: Config) {
    let _ = thread::spawn(|| {
        let mut runtime = tokio::runtime::Builder::new()
            // single threaded
            .basic_scheduler()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            if let Err(e) = init_future(config).await {
                log::warn!("{}", e);
            }
        });
    });
}

pub async fn init_future(config: Config) -> Result<(), Error> {
    let (recorder, task) = new(config);
    metrics::set_boxed_recorder(Box::new(recorder)).map_err(Error::SetRecorder)?;
    task.await;
    Ok(())
}

pub fn new(config: Config) -> (RecorderHandle, impl Future<Output = ()>) {
    let (collect_sender, collect_receiver) = mpsc::channel(1024);
    let (emit_sender, emit_receiver) = mpsc::channel(1024);
    let mut message_stream = Box::pin(
        stream::select(
            collect_receiver.map(Message::Datum),
            mk_send_batch_timer(emit_sender.clone(), &config),
        )
        .take_until(config.shutdown_signal.clone().map(|_| true)),
    );

    let emitter = mk_emitter(emit_receiver, config.client, config.cloudwatch_namespace);

    let internal_config = CollectorConfig {
        default_dimensions: config.default_dimensions,
        storage_resolution: config.storage_resolution,
    };

    let mut collector = Collector::new(internal_config);
    let collection_fut = async move {
        while let Some(msg) = message_stream.next().await {
            collector.accept(msg);
        }
        // Need to drop this before flushing or we deadlock on shutdown
        drop(message_stream);
        // Send a final flush on shutdown
        collector.accept(Message::SendBatch {
            send_all_before: std::u64::MAX,
            emit_sender,
        });
    };
    (
        RecorderHandle(collect_sender),
        future::join(collection_fut, emitter.map(|_| ())).map(|_| ()),
    )
}

fn mk_emitter(
    mut emit_receiver: mpsc::Receiver<Vec<MetricDatum>>,
    cloudwatch_client: Box<dyn CloudWatch + Send + Sync>,
    cloudwatch_namespace: String,
) -> impl Future<Output = ()> {
    async move {
        let put = |metric_data| {
            let cloudwatch_namespace = cloudwatch_namespace.clone();
            async {
                let send_fut = cloudwatch_client.put_metric_data(PutMetricDataInput {
                    metric_data,
                    namespace: cloudwatch_namespace,
                });
                match tokio::time::timeout(SEND_TIMEOUT, send_fut).await {
                    Ok(Ok(())) => log::debug!("Successfully sent a metrics batch to CloudWatch."),
                    Ok(Err(e)) => log::warn!("Failed to send metrics: {}", e),
                    Err(tokio::time::Elapsed { .. }) => {
                        log::warn!("Failed to send metrics: send timeout")
                    }
                }
            }
        };
        while let Some(metrics) = emit_receiver.next().await {
            future::join_all(metrics_chunks(&metrics).map(put))
                .map(|_| ())
                .await;
        }
    }
}

fn count_option_vec<T>(vs: &Option<Vec<T>>) -> usize {
    vs.as_ref().map(|vs| vs.len()).unwrap_or(0)
}

const MAX_CW_METRICS_PUT_SIZE: usize = 40_000;

fn metrics_chunks(mut metrics: &[MetricDatum]) -> impl Iterator<Item = Vec<MetricDatum>> + '_ {
    std::iter::from_fn(move || {
        let mut split = 0;

        let mut current_len = 0;
        // PutMetricData uses this really high overhead format so just take a high estimate of that.
        //
        // Assumes each value sent is ~60 bytes
        // ```
        // MetricData.member.2.Dimensions.member.2.Value=m1.small
        // ```
        for (i, metric) in metrics.iter().take(MAX_CW_METRICS_PER_CALL).enumerate() {
            current_len += metric_size(metric);
            if current_len > MAX_CW_METRICS_PUT_SIZE {
                break;
            }
            split = i + 1;
        }
        let (chunk, rest) = metrics.split_at(split);
        metrics = rest;
        if chunk.is_empty() {
            None
        } else {
            Some(chunk.to_owned())
        }
    })
}

fn metric_size(metric: &MetricDatum) -> usize {
    let MetricDatum {
        counts,
        values,
        dimensions,
        // 6 fields
        metric_name: _,
        statistic_values: _,
        storage_resolution: _,
        timestamp: _,
        unit: _,
        value: _,
    } = metric;
    60 * (
        // The 6 non Vec fields
        6 + count_option_vec(values) + count_option_vec(counts) + count_option_vec(dimensions)
    )
}

fn mk_send_batch_timer(
    emit_sender: mpsc::Sender<Vec<MetricDatum>>,
    config: &Config,
) -> impl Stream<Item = Message> {
    let interval = Duration::from_secs(config.send_interval_secs);
    let storage_resolution = config.storage_resolution;
    tokio::time::interval_at(tokio::time::Instant::now(), interval).map(move |_instant| {
        let send_all_before = time_key(current_timestamp(), storage_resolution) - 1;
        Message::SendBatch {
            send_all_before,
            emit_sender: emit_sender.clone(),
        }
    })
}

fn current_timestamp() -> Timestamp {
    time::UNIX_EPOCH.elapsed().unwrap().as_secs()
}

fn timestamp_string(now: SystemTime) -> String {
    let dt = chrono::DateTime::<chrono::offset::Utc>::from(now);
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn time_key(timestamp: Timestamp, resolution: Resolution) -> Timestamp {
    match resolution {
        Resolution::Second => timestamp,
        Resolution::Minute => timestamp - (timestamp % 60),
    }
}

impl Collector {
    fn new(config: CollectorConfig) -> Self {
        Self {
            metrics_data: Default::default(),
            config,
        }
    }

    fn accept(&mut self, message: Message) {
        let result = match message {
            Message::Datum(datum) => Ok(self.accept_datum(datum)),
            Message::SendBatch {
                send_all_before,
                emit_sender,
            } => self.accept_send_batch(send_all_before, emit_sender),
        };
        if let Err(e) = result {
            log::warn!("Failed to accept message: {}", e);
        }
    }

    fn accept_datum(&mut self, datum: Datum) {
        let aggregate = self
            .metrics_data
            .entry(time_key(
                current_timestamp(),
                self.config.storage_resolution,
            ))
            .or_insert_with(HashMap::new)
            .entry(datum.key)
            .or_default();

        match datum.value {
            Value::Counter(value) => {
                let counter = &mut aggregate.counter;
                counter.sample_count += 1;
                counter.sum += value;
            }
            Value::Gauge(value) => {
                let value = value as f64;
                let gauge = &mut aggregate.gauge;
                gauge.sample_count += 1.0;
                gauge.sum += value;
                gauge.maximum = gauge.maximum.max(value);
                gauge.minimum = gauge.minimum.min(value);
            }
            Value::Histogram(value) => {
                *aggregate.histogram.entry(value).or_default() += 1;
            }
        }
    }

    fn dimensions(&self, key: &Key) -> Vec<Dimension> {
        let dimensions_from_keys = key.labels().map(|l| Dimension {
            name: l.key().to_owned(),
            value: l.value().to_owned(),
        });
        self.default_dimensions()
            .chain(dimensions_from_keys)
            .take(MAX_CLOUDWATCH_DIMENSIONS)
            .collect()
    }

    /// Sends a batch of the earliest collected metrics to CloudWatch
    ///
    /// # Params
    /// * send_all_before: All messages before this timestamp should be split off from the aggregation and
    /// sent to CloudWatch
    fn accept_send_batch(
        &mut self,
        send_all_before: Timestamp,
        mut emit_sender: mpsc::Sender<Vec<MetricDatum>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut range = self.metrics_data.split_off(&send_all_before);
        mem::swap(&mut range, &mut self.metrics_data);

        let mut metrics_batch = vec![];

        for (timestamp, stats_by_key) in range {
            let timestamp = timestamp_string(time::UNIX_EPOCH + Duration::from_secs(timestamp));

            for (key, aggregate) in stats_by_key {
                let Aggregate {
                    counter,
                    gauge,
                    histogram,
                } = aggregate;
                let dimensions = self.dimensions(&key);

                let stats_set_datum = &mut |stats_set, unit| MetricDatum {
                    dimensions: Some(dimensions.clone()),
                    metric_name: key.name().into_owned(),
                    timestamp: Some(timestamp.clone()),
                    storage_resolution: Some(self.config.storage_resolution.as_secs()),
                    statistic_values: Some(stats_set),
                    unit,
                    ..Default::default()
                };

                if counter.sample_count > 0 {
                    let sum = counter.sum as f64;
                    let stats_set = StatisticSet {
                        sample_count: counter.sample_count as f64,
                        sum,
                        // Max and min for a count can either be the sum or the max/min of the
                        // value passed to each `increment_counter` call.
                        //
                        // In the case where we only increment by `1` each call the latter makes
                        // min and max basically useless since the end result will leave both as `1`.
                        // In the case where we sum the count first before calling
                        // `increment_counter` we do lose some granularity as the latter would give
                        // a spread in min/max.
                        // However if that is an interesting metric it would often be
                        // better modeled as the gauge (measuring how much were processed in each
                        // batch).
                        //
                        // Therefor we opt to send the sum to give a measure of how many
                        // counts *this* metrics instance observed in this time period.
                        maximum: sum,
                        minimum: sum,
                    };
                    metrics_batch.push(stats_set_datum(stats_set, Some("Count".to_owned())));
                }
                if gauge.sample_count > 0.0 {
                    metrics_batch.push(stats_set_datum(gauge, None));
                }

                let histogram_datum = &mut |Histogram { values, counts }, unit| MetricDatum {
                    dimensions: Some(dimensions.clone()),
                    metric_name: key.name().into_owned(),
                    timestamp: Some(timestamp.clone()),
                    storage_resolution: Some(self.config.storage_resolution.as_secs()),
                    unit,
                    values: Some(values),
                    counts: Some(counts),
                    ..Default::default()
                };

                if !histogram.is_empty() {
                    let histogram_data = &mut histogram.into_iter().map(|(k, v)| HistogramDatum {
                        value: k as f64,
                        count: v as f64,
                    });
                    loop {
                        let histogram = histogram_data.take(MAX_HISTOGRAM_VALUES).fold(
                            Histogram::default(),
                            |mut memo, datum| {
                                memo.values.push(datum.value);
                                memo.counts.push(datum.count);
                                memo
                            },
                        );
                        if histogram.values.is_empty() {
                            break;
                        };
                        metrics_batch.push(histogram_datum(histogram, None));
                    }
                }
            }
        }
        if !metrics_batch.is_empty() {
            emit_sender.try_send(metrics_batch)?;
        }
        Ok(())
    }

    fn default_dimensions(&self) -> impl Iterator<Item = Dimension> {
        self.config
            .default_dimensions
            .clone()
            .into_iter()
            .map(|(name, value)| Dimension { name, value })
    }
}

impl Recorder for RecorderHandle {
    fn increment_counter(&self, key: Key, value: u64) {
        let _ = self.0.clone().try_send(Datum {
            key,
            value: Value::Counter(value),
        });
    }

    fn update_gauge(&self, key: Key, value: i64) {
        let _ = self.0.clone().try_send(Datum {
            key,
            value: Value::Gauge(value),
        });
    }

    fn record_histogram(&self, key: Key, value: u64) {
        let _ = self.0.clone().try_send(Datum {
            key,
            value: Value::Histogram(value),
        });
    }
}

impl Resolution {
    fn as_secs(self) -> i64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use proptest::prelude::*;

    #[test]
    fn time_key_should_truncate() {
        assert_eq!(time_key(370, Resolution::Second), 370);
        assert_eq!(time_key(370, Resolution::Minute), 360);
    }

    fn metrics() -> impl Strategy<Value = Vec<MetricDatum>> {
        let values = || {
            proptest::collection::vec(proptest::num::f64::ANY, 1..MAX_HISTOGRAM_VALUES)
                .prop_map(Some)
        };
        let timestamp = timestamp_string(time::UNIX_EPOCH);
        let datum = (
            values(),
            values(),
            proptest::collection::vec(
                ("name", "value").prop_map(|(name, value)| Dimension { name, value }),
                1..6,
            )
            .prop_map(Some),
        )
            .prop_map(move |(counts, values, dimensions)| MetricDatum {
                counts,
                values,
                dimensions,
                metric_name: "test".into(),
                statistic_values: Some(StatisticSet::default()),
                storage_resolution: Some(1),
                timestamp: Some(timestamp.clone()),
                value: Some(1.0),
                unit: Some("Count".into()),
            });

        proptest::collection::vec(datum, 1..100)
    }

    #[test]
    fn chunks_fit_in_cloudwatch_constraints() {
        proptest! {
            proptest::prelude::ProptestConfig { cases: 30, .. Default::default() },
            |(metrics in metrics())| {
                for metric_data in metrics_chunks(&metrics) {
                    assert!(metric_data.len() > 0 && metric_data.len() < MAX_CW_METRICS_PER_CALL, "Sending too many metrics per call: {}", metric_data.len());
                    let estimated_size = metric_data.iter().map(metric_size).sum::<usize>();
                    assert!(estimated_size < MAX_CW_METRICS_PUT_SIZE, "{} >= {}", estimated_size, MAX_CW_METRICS_PUT_SIZE);
                }
            }
        }
    }
}
